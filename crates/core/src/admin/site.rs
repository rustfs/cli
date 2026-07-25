//! Service control and site replication admin types.
//!
//! Wire formats mirror the RustFS server's `crates/madmin` definitions
//! (MinIO-admin-compatible JSON field names).

use std::collections::BTreeMap;
use std::fmt;

use rustls_pki_types::{CertificateDer, pem::PemObject};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use x509_parser::parse_x509_certificate;

use crate::error::{Error, Result};

/// Maximum successful site replication response accepted from the server.
pub const MAX_SITE_REPLICATION_SUCCESS_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum site replication error response accepted from the server.
pub const MAX_SITE_REPLICATION_ERROR_RESPONSE_BYTES: usize = 64 * 1024;
/// Maximum serialized site replication edit request.
pub const MAX_SITE_REPLICATION_REQUEST_BYTES: usize = 1024 * 1024;
/// Maximum custom CA bundle accepted by the CLI.
pub const MAX_SITE_REPLICATION_CA_CERT_BYTES: usize = 256 * 1024;
/// Stable capability name for the durable site-replication repair lifecycle.
pub const SITE_REPLICATION_REPAIR_CAPABILITY: &str = "admin.site-replication.repair";
/// Maximum preflight/operation response accepted from the repair routes.
pub const MAX_SITE_REPLICATION_REPAIR_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Validate the opaque HMAC-SHA256 v1 preflight token without inspecting its contents.
pub fn validate_site_replication_repair_token(token: &str) -> Result<()> {
    if token.len() == 43
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(Error::InvalidPath(
            "Preflight token must be the complete 43-character server-issued token".to_string(),
        ))
    }
}

/// Validate the canonical UUID syntax required for durable operation identifiers.
pub fn validate_site_replication_repair_operation_id(operation_id: &str) -> Result<()> {
    let bytes = operation_id.as_bytes();
    let valid = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidPath(
            "Operation ID must be a canonical UUID".to_string(),
        ))
    }
}

/// Capability contract advertised by `/rustfs/admin/v4/runtime/capabilities`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SiteReplicationRepairCapabilityContract {
    pub contract_version: u32,
    pub status: super::RuntimeCapabilityStatus,
    pub modes: Vec<String>,
    pub execute_route: String,
    pub status_route: String,
    pub preflight_token_contract: String,
    pub operation_id_format: String,
    pub max_retained_successful_operations: usize,
    pub disabled_by_default: bool,
}

impl SiteReplicationRepairCapabilityContract {
    /// Reject incomplete or incompatible server advertisements before using a repair route.
    pub fn validate(&self) -> Result<()> {
        let supported = self.status.state == super::RuntimeCapabilityState::Supported;
        let modes_ok = self.modes.len() == 2
            && ["dry-run", "execute"]
                .iter()
                .all(|required| self.modes.iter().any(|mode| mode == required));
        if self.contract_version != 1
            || !supported
            || !modes_ok
            || self.execute_route != "/rustfs/admin/v3/site-replication/repair"
            || self.status_route != "/rustfs/admin/v3/site-replication/repair/status"
            || self.preflight_token_contract != "hmac-sha256-v1"
            || self.operation_id_format != "uuid"
            || self.max_retained_successful_operations == 0
            || self.disabled_by_default
        {
            return Err(Error::UnsupportedFeature(
                "RustFS did not advertise the supported durable site-replication repair contract"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

/// Request body for dry-run and execute repair operations.
#[derive(Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SiteReplicationRepairRequest {
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preflight_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
}

/// One durable task checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SiteReplicationRepairTaskStatus {
    pub task_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Per-family task counts and checkpoints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SiteReplicationRepairFamilyStatus {
    pub planned: usize,
    pub succeeded: usize,
    pub failed: usize,
    #[serde(default)]
    pub retry_events: usize,
    #[serde(default)]
    pub tasks: Vec<SiteReplicationRepairTaskStatus>,
    #[serde(default)]
    pub errors: Vec<String>,
}

/// Per-site repair plan or operation status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SiteReplicationRepairSiteStatus {
    pub deployment_id: String,
    pub name: String,
    pub families: BTreeMap<String, SiteReplicationRepairFamilyStatus>,
}

/// Dry-run response containing the server-issued preflight token.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SiteReplicationRepairPreflight {
    pub mode: String,
    pub status: String,
    pub preflight_token: String,
    pub retry_events: usize,
    pub sites: BTreeMap<String, SiteReplicationRepairSiteStatus>,
}

impl fmt::Debug for SiteReplicationRepairPreflight {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SiteReplicationRepairPreflight")
            .field("mode", &self.mode)
            .field("status", &self.status)
            .field("has_preflight_token", &!self.preflight_token.is_empty())
            .field("retry_events", &self.retry_events)
            .field("site_count", &self.sites.len())
            .finish()
    }
}

impl SiteReplicationRepairPreflight {
    pub fn validate(&self) -> Result<()> {
        if self.mode != "dry-run" || self.status != "planned" {
            return Err(Error::General(
                "RustFS returned an inconsistent site-replication repair preflight".to_string(),
            ));
        }
        validate_site_replication_repair_token(&self.preflight_token)?;
        validate_repair_sites(&self.sites)
    }
}

/// Durable execute/status snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SiteReplicationRepairOperationStatus {
    pub mode: String,
    pub operation_id: String,
    pub status: String,
    pub sites: BTreeMap<String, SiteReplicationRepairSiteStatus>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
}

impl SiteReplicationRepairOperationStatus {
    pub fn validate(&self, expected_operation_id: &str) -> Result<()> {
        validate_site_replication_repair_operation_id(&self.operation_id)?;
        if self.mode != "execute"
            || !self
                .operation_id
                .eq_ignore_ascii_case(expected_operation_id)
            || !matches!(
                self.status.as_str(),
                "running" | "success" | "partial" | "failed"
            )
        {
            return Err(Error::General(
                "RustFS returned an inconsistent site-replication repair operation".to_string(),
            ));
        }
        for timestamp in [&self.created_at, &self.updated_at, &self.completed_at]
            .into_iter()
            .flatten()
        {
            timestamp.parse::<jiff::Timestamp>().map_err(|_| {
                Error::General(
                    "RustFS returned an invalid site-replication repair timestamp".to_string(),
                )
            })?;
        }
        if self.created_at.is_none() || self.updated_at.is_none() {
            return Err(Error::General(
                "RustFS returned an incomplete site-replication repair timestamp".to_string(),
            ));
        }
        validate_repair_sites(&self.sites)
    }

    /// Terminal snapshots that must cause a non-zero CLI exit.
    pub fn has_failure(&self) -> bool {
        matches!(
            self.status.trim().to_ascii_lowercase().as_str(),
            "partial" | "failed" | "failure"
        ) || self
            .sites
            .values()
            .flat_map(|site| site.families.values())
            .any(|family| {
                family.failed > 0
                    || family.tasks.iter().any(|task| {
                        matches!(
                            task.status.trim().to_ascii_lowercase().as_str(),
                            "failed" | "failure"
                        )
                    })
            })
    }
}

fn validate_repair_sites(sites: &BTreeMap<String, SiteReplicationRepairSiteStatus>) -> Result<()> {
    for (deployment_id, site) in sites {
        if deployment_id.is_empty()
            || site.deployment_id != *deployment_id
            || site.name.trim().is_empty()
            || site.families.is_empty()
        {
            return Err(Error::General(
                "RustFS returned an incomplete site-replication repair site".to_string(),
            ));
        }
        for (family_name, family) in &site.families {
            let successful = family
                .tasks
                .iter()
                .filter(|task| matches!(task.status.as_str(), "succeeded" | "skipped"))
                .count();
            let failed = family
                .tasks
                .iter()
                .filter(|task| task.status == "failed")
                .count();
            let mut task_ids = std::collections::BTreeSet::new();
            if family_name.is_empty()
                || family.succeeded.saturating_add(family.failed) > family.planned
                || family.tasks.len() != family.planned
                || successful != family.succeeded
                || failed != family.failed
                || family
                    .errors
                    .iter()
                    .any(|error| !repair_error_is_safe(error))
                || family.tasks.iter().any(|task| {
                    validate_site_replication_repair_token(&task.task_id).is_err()
                        || !task_ids.insert(task.task_id.as_str())
                        || !matches!(
                            task.status.as_str(),
                            "planned" | "running" | "succeeded" | "failed" | "skipped"
                        )
                        || task
                            .error
                            .as_deref()
                            .is_some_and(|error| !repair_error_is_safe(error))
                })
            {
                return Err(Error::General(
                    "RustFS returned inconsistent site-replication repair checkpoints".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn repair_error_is_safe(error: &str) -> bool {
    matches!(
        error,
        "authorization-failed"
            | "remote-timeout"
            | "remote-dns-failed"
            | "remote-tls-failed"
            | "remote-connect-failed"
            | "remote-operation-failed"
    )
}

/// Durable site-replication repair transport.
#[async_trait::async_trait]
pub trait SiteReplicationRepairApi: Send + Sync {
    async fn site_replication_repair_capability(
        &self,
    ) -> Result<SiteReplicationRepairCapabilityContract>;
    async fn site_replication_repair_dry_run(&self) -> Result<SiteReplicationRepairPreflight>;
    async fn site_replication_repair_execute(
        &self,
        preflight_token: &str,
        operation_id: &str,
    ) -> Result<SiteReplicationRepairOperationStatus>;
    async fn site_replication_repair_status(
        &self,
        operation_id: &str,
    ) -> Result<SiteReplicationRepairOperationStatus>;
}

/// Validate a bounded certificate-only PEM bundle for site replication edits.
pub fn validate_site_replication_ca_bundle(pem: &[u8]) -> Result<()> {
    if pem.is_empty() {
        return Err(Error::InvalidPath(
            "CA certificate bundle must not be empty".to_string(),
        ));
    }
    if pem.len() > MAX_SITE_REPLICATION_CA_CERT_BYTES {
        return Err(Error::InvalidPath(format!(
            "CA certificate bundle exceeds the {MAX_SITE_REPLICATION_CA_CERT_BYTES} byte limit"
        )));
    }

    let text = std::str::from_utf8(pem)
        .map_err(|_| Error::InvalidPath("CA certificate bundle must be UTF-8 PEM".to_string()))?;
    let mut inside_certificate = false;
    let mut certificates = 0_usize;
    for line in text.lines() {
        let line = line.trim();
        if line == "-----BEGIN CERTIFICATE-----" {
            if inside_certificate {
                return Err(Error::InvalidPath(
                    "CA certificate bundle contains nested PEM blocks".to_string(),
                ));
            }
            inside_certificate = true;
            certificates += 1;
        } else if line == "-----END CERTIFICATE-----" {
            if !inside_certificate {
                return Err(Error::InvalidPath(
                    "CA certificate bundle contains an unmatched PEM end marker".to_string(),
                ));
            }
            inside_certificate = false;
        } else if line.starts_with("-----BEGIN ") || line.starts_with("-----END ") {
            return Err(Error::InvalidPath(
                "CA bundle may contain CERTIFICATE PEM blocks only".to_string(),
            ));
        } else if !inside_certificate && !line.is_empty() {
            return Err(Error::InvalidPath(
                "CA bundle contains data outside CERTIFICATE PEM blocks".to_string(),
            ));
        }
    }
    if inside_certificate || certificates == 0 {
        return Err(Error::InvalidPath(
            "CA bundle must contain complete CERTIFICATE PEM blocks".to_string(),
        ));
    }

    let decoded = CertificateDer::pem_slice_iter(pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| Error::InvalidPath("CA certificate bundle is malformed".to_string()))?;
    if decoded.len() != certificates {
        return Err(Error::InvalidPath(
            "CA certificate bundle contains malformed certificate data".to_string(),
        ));
    }
    for certificate in decoded {
        let (remaining, _) = parse_x509_certificate(certificate.as_ref()).map_err(|_| {
            Error::InvalidPath("CA certificate bundle contains invalid X.509 DER".to_string())
        })?;
        if !remaining.is_empty() {
            return Err(Error::InvalidPath(
                "CA certificate bundle contains trailing DER data".to_string(),
            ));
        }
    }
    Ok(())
}

/// A peer site definition for `site-replication/add`.
///
/// Field names follow the MinIO admin wire format (note: the endpoint
/// serializes as `endpoints`).
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct PeerSiteSpec {
    #[serde(default)]
    pub name: String,
    #[serde(rename = "endpoints", default)]
    pub endpoint: String,
    #[serde(rename = "accessKey", default)]
    pub access_key: String,
    #[serde(rename = "secretKey", default)]
    pub secret_key: String,
    #[serde(rename = "skipTlsVerify", default)]
    pub skip_tls_verify: bool,
    #[serde(
        rename = "caCertPem",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub ca_cert_pem: String,
}

impl fmt::Debug for PeerSiteSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PeerSiteSpec")
            .field("name", &self.name)
            .field("endpoint", &self.endpoint)
            .field("has_access_key", &!self.access_key.is_empty())
            .field("has_secret_key", &!self.secret_key.is_empty())
            .field("skip_tls_verify", &self.skip_tls_verify)
            .field("has_custom_ca", &!self.ca_cert_pem.is_empty())
            .finish()
    }
}

/// Complete editable peer snapshot returned by `site-replication/info`.
///
/// The original object is retained so omitted fields and future server fields
/// survive a read-modify-write edit. Typed accessors are the only supported way
/// to inspect or mutate known fields. Custom debug output never prints the CA or
/// arbitrary field values.
#[derive(Clone, PartialEq, Default)]
pub struct SiteReplicationPeer {
    document: Map<String, Value>,
}

impl<'de> Deserialize<'de> for SiteReplicationPeer {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let document = value
            .as_object()
            .cloned()
            .ok_or_else(|| serde::de::Error::custom("site replication peer must be an object"))?;
        Ok(Self { document })
    }
}

impl Serialize for SiteReplicationPeer {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.document.serialize(serializer)
    }
}

impl SiteReplicationPeer {
    pub fn endpoint(&self) -> Option<&str> {
        self.string_field("endpoint")
            .filter(|endpoint| !endpoint.is_empty())
            .or_else(|| {
                self.string_field("endpoints")
                    .filter(|endpoint| !endpoint.is_empty())
            })
    }

    pub fn name(&self) -> Option<&str> {
        self.string_field("name")
    }

    pub fn deployment_id(&self) -> Option<&str> {
        self.string_field("deploymentID")
    }

    pub fn sync(&self) -> Option<&str> {
        self.string_field("sync")
    }

    pub fn default_bandwidth(&self) -> Option<&Value> {
        self.document.get("defaultbandwidth")
    }

    pub fn replicate_ilm_expiry(&self) -> Option<bool> {
        self.document
            .get("replicate-ilm-expiry")
            .and_then(Value::as_bool)
    }

    pub fn object_naming_mode(&self) -> Option<&str> {
        self.string_field("objectNamingMode")
    }

    pub fn skip_tls_verify(&self) -> Option<bool> {
        self.document.get("skipTlsVerify").and_then(Value::as_bool)
    }

    pub fn ca_cert_pem(&self) -> Option<&str> {
        self.string_field("caCertPem")
    }

    pub fn api_version(&self) -> Option<&str> {
        self.string_field("apiVersion")
    }

    pub fn has_custom_ca(&self) -> bool {
        self.ca_cert_pem().is_some_and(|pem| !pem.is_empty())
    }

    pub fn set_endpoint(&mut self, endpoint: String) {
        self.document
            .insert("endpoint".to_string(), Value::String(endpoint));
    }

    pub fn set_name(&mut self, name: String) {
        self.document
            .insert("name".to_string(), Value::String(name));
    }

    pub fn set_skip_tls_verify(&mut self, skip_tls_verify: bool) {
        self.document
            .insert("skipTlsVerify".to_string(), Value::Bool(skip_tls_verify));
    }

    pub fn set_ca_cert_pem(&mut self, ca_cert_pem: String) {
        self.document
            .insert("caCertPem".to_string(), Value::String(ca_cert_pem));
    }

    fn string_field(&self, name: &str) -> Option<&str> {
        self.document.get(name).and_then(Value::as_str)
    }
}

impl fmt::Debug for SiteReplicationPeer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SiteReplicationPeer")
            .field("has_endpoint", &self.endpoint().is_some())
            .field("has_name", &self.name().is_some())
            .field("has_deployment_id", &self.deployment_id().is_some())
            .field("skip_tls_verify", &self.skip_tls_verify())
            .field("has_custom_ca", &self.has_custom_ca())
            .field("field_count", &self.document.len())
            .finish()
    }
}

/// Typed site replication information with server credentials discarded.
#[derive(Clone, PartialEq, Default)]
pub struct SiteReplicationInfo {
    pub enabled: bool,
    pub name: String,
    pub sites: Vec<SiteReplicationPeer>,
    pub api_version: String,
    pub extensions: BTreeMap<String, Value>,
}

impl<'de> Deserialize<'de> for SiteReplicationInfo {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            enabled: bool,
            #[serde(default)]
            name: String,
            #[serde(default)]
            sites: Vec<SiteReplicationPeer>,
            #[serde(rename = "serviceAccountAccessKey", default)]
            _service_account_access_key: Option<serde::de::IgnoredAny>,
            #[serde(rename = "apiVersion", default)]
            api_version: String,
            #[serde(flatten)]
            extensions: BTreeMap<String, Value>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            enabled: wire.enabled,
            name: wire.name,
            sites: wire.sites,
            api_version: wire.api_version,
            extensions: wire.extensions,
        })
    }
}

impl fmt::Debug for SiteReplicationInfo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SiteReplicationInfo")
            .field("enabled", &self.enabled)
            .field("has_name", &!self.name.is_empty())
            .field("sites", &self.sites)
            .field("has_api_version", &!self.api_version.is_empty())
            .field("extension_count", &self.extensions.len())
            .finish()
    }
}

impl SiteReplicationInfo {
    /// Resolve an exact deployment ID, then a unique exact site name.
    pub fn resolve_peer(&self, selector: &str) -> Result<&SiteReplicationPeer> {
        if let Some(peer) = self
            .sites
            .iter()
            .find(|peer| peer.deployment_id() == Some(selector))
        {
            return Ok(peer);
        }

        let mut matching_names = self
            .sites
            .iter()
            .filter(|peer| peer.name() == Some(selector));
        let peer = matching_names.next().ok_or_else(|| {
            Error::NotFound(format!(
                "site '{selector}' does not match an exact deployment ID or site name"
            ))
        })?;
        if matching_names.next().is_some() {
            return Err(Error::Conflict(format!(
                "site name '{selector}' matches multiple deployments; use a deployment ID"
            )));
        }
        Ok(peer)
    }
}

/// Operation accepted by `site-replication/resync/op`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SiteReplicationResyncOperation {
    Start,
    Status,
    Cancel,
}

impl SiteReplicationResyncOperation {
    /// Exact query value expected by the RustFS admin route.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Status => "status",
            Self::Cancel => "cancel",
        }
    }

    /// Whether this operation changes server-side resync state.
    pub const fn is_mutation(self) -> bool {
        matches!(self, Self::Start | Self::Cancel)
    }
}

/// Per-bucket result returned by a site replication resync operation.
#[derive(Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SiteReplicationResyncBucketStatus {
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub status: String,
    #[serde(rename = "errorDetail", default)]
    pub error_detail: String,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl SiteReplicationResyncBucketStatus {
    /// Detect a failed bucket even when the server's status summary is stale.
    pub fn has_failure(&self) -> bool {
        status_is_failed(&self.status) || !self.error_detail.trim().is_empty()
    }
}

impl fmt::Debug for SiteReplicationResyncBucketStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SiteReplicationResyncBucketStatus")
            .field("has_bucket", &!self.bucket.is_empty())
            .field("has_status", &!self.status.is_empty())
            .field("has_error_detail", &!self.error_detail.is_empty())
            .field("extension_count", &self.extensions.len())
            .finish()
    }
}

/// Typed response from `site-replication/resync/op`.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct SiteReplicationResyncStatus {
    #[serde(rename = "op", default)]
    pub operation: String,
    #[serde(rename = "id", default)]
    pub resync_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub buckets: Vec<SiteReplicationResyncBucketStatus>,
    #[serde(rename = "errorDetail", default)]
    pub error_detail: String,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
    #[serde(skip)]
    semantic_failure: Option<bool>,
    #[serde(skip)]
    semantic_not_found: bool,
}

impl SiteReplicationResyncStatus {
    /// Freeze protocol semantics before credentials are redacted for output.
    ///
    /// Once captured, the semantic result intentionally remains stable even if
    /// the public wire fields are replaced by their safe output projections.
    pub fn capture_semantics(&mut self) {
        self.semantic_failure = Some(self.compute_failure());
        self.semantic_not_found = self.status.trim().eq_ignore_ascii_case("not-found");
    }

    /// Detect overall and partial failures without trusting a success summary.
    pub fn has_failure(&self) -> bool {
        self.semantic_failure
            .unwrap_or_else(|| self.compute_failure())
    }

    /// Detect the wire-level no-snapshot marker after output sanitization.
    pub fn is_not_found(&self) -> bool {
        self.semantic_not_found || self.status.trim().eq_ignore_ascii_case("not-found")
    }

    fn compute_failure(&self) -> bool {
        !status_is_success(&self.status)
            || !self.error_detail.trim().is_empty()
            || self
                .buckets
                .iter()
                .any(SiteReplicationResyncBucketStatus::has_failure)
    }
}

impl PartialEq for SiteReplicationResyncStatus {
    fn eq(&self, other: &Self) -> bool {
        self.operation == other.operation
            && self.resync_id == other.resync_id
            && self.status == other.status
            && self.buckets == other.buckets
            && self.error_detail == other.error_detail
            && self.extensions == other.extensions
    }
}

impl fmt::Debug for SiteReplicationResyncStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SiteReplicationResyncStatus")
            .field("has_operation", &!self.operation.is_empty())
            .field("has_resync_id", &!self.resync_id.is_empty())
            .field("has_status", &!self.status.is_empty())
            .field("bucket_count", &self.buckets.len())
            .field("has_error_detail", &!self.error_detail.is_empty())
            .field("extension_count", &self.extensions.len())
            .finish()
    }
}

fn status_is_failed(status: &str) -> bool {
    status.trim().eq_ignore_ascii_case("failed")
}

fn status_is_success(status: &str) -> bool {
    status.trim().eq_ignore_ascii_case("success")
}

/// Typed response from `site-replication/edit`.
#[derive(Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ReplicateEditStatus {
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub status: String,
    #[serde(rename = "errorDetail", default)]
    pub error_detail: String,
    #[serde(rename = "apiVersion", default)]
    pub api_version: String,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl fmt::Debug for ReplicateEditStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplicateEditStatus")
            .field("success", &self.success)
            .field("has_status", &!self.status.is_empty())
            .field("has_error_detail", &!self.error_detail.is_empty())
            .field("has_api_version", &!self.api_version.is_empty())
            .field("extension_count", &self.extensions.len())
            .finish()
    }
}

/// Response from `POST /service?action=...`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServiceActionResult {
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub accepted: bool,
    /// Whether the action takes real effect on this build (vs advisory only).
    #[serde(default)]
    pub effective: bool,
    #[serde(default)]
    pub message: String,
}

/// Options for `site-replication/status`.
#[derive(Debug, Clone, Default)]
pub struct SiteStatusOptions {
    pub buckets: bool,
    pub users: bool,
    pub groups: bool,
    pub policies: bool,
    pub metrics: bool,
    pub peer_state: bool,
    pub ilm_expiry_rules: bool,
}

/// Request body for `site-replication/remove`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SiteRemoveSpec {
    #[serde(rename = "sites", default, skip_serializing_if = "Vec::is_empty")]
    pub site_names: Vec<String>,
    #[serde(rename = "all", default)]
    pub remove_all: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE_INFO: &str = r#"{
        "enabled": true,
        "name": "primary",
        "sites": [{
            "endpoint": "https://secondary.example.test",
            "name": "secondary",
            "deploymentID": "deployment-2",
            "sync": "enable",
            "defaultbandwidth": {
                "bandwidthLimitPerBucket": 1048576,
                "set": true,
                "updatedAt": "2026-07-22T00:00:00Z",
                "futureBandwidth": "preserved"
            },
            "replicate-ilm-expiry": true,
            "objectNamingMode": "path",
            "skipTlsVerify": false,
            "caCertPem": "-----BEGIN CERTIFICATE-----\nSAFE-TO-SEND-NOT-SAFE-TO-PRINT\n-----END CERTIFICATE-----\n",
            "apiVersion": "v1",
            "futurePeer": {
                "mode": "preserved",
                "accessToken": "OPAQUE-TOKEN-MUST-NOT-PRINT",
                "nested": [{"safe": "preserved", "sessionToken": "OPAQUE-TOKEN-MUST-NOT-PRINT"}]
            }
        }],
        "serviceAccountAccessKey": "SHOULD-NEVER-BE-EXPOSED",
        "apiVersion": "v1",
        "futureInfo": "preserved",
        "secretFuture": "OPAQUE-TOKEN-MUST-NOT-PRINT"
    }"#;

    #[test]
    fn site_info_discards_wrapper_key_but_preserves_opaque_future_fields() {
        let info: SiteReplicationInfo =
            serde_json::from_str(SITE_INFO).expect("valid site information fixture");

        let debug = format!("{info:?}");

        assert!(!debug.contains("SHOULD-NEVER-BE-EXPOSED"));
        assert!(!debug.contains("SAFE-TO-SEND-NOT-SAFE-TO-PRINT"));
        assert!(!debug.contains("secretFuture"));
        assert!(!debug.contains("accessToken"));
        assert_eq!(info.extensions["futureInfo"], "preserved");
        assert_eq!(
            info.extensions["secretFuture"],
            "OPAQUE-TOKEN-MUST-NOT-PRINT"
        );
        let wire = serde_json::to_value(&info.sites[0]).expect("peer is serializable");
        assert_eq!(wire["futurePeer"]["mode"], "preserved");
        assert_eq!(wire["futurePeer"]["nested"][0]["safe"], "preserved");
        assert_eq!(
            wire["futurePeer"]["nested"][0]["sessionToken"],
            "OPAQUE-TOKEN-MUST-NOT-PRINT"
        );
    }

    #[test]
    fn peer_round_trip_preserves_edit_fields_and_opaque_future_fields() {
        let info: SiteReplicationInfo =
            serde_json::from_str(SITE_INFO).expect("valid site information fixture");
        let peer = &info.sites[0];
        let wire = serde_json::to_value(peer).expect("peer is serializable");

        assert_eq!(wire["deploymentID"], "deployment-2");
        assert_eq!(wire["sync"], "enable");
        assert_eq!(wire["defaultbandwidth"]["bandwidthLimitPerBucket"], 1048576);
        assert_eq!(wire["defaultbandwidth"]["futureBandwidth"], "preserved");
        assert_eq!(wire["replicate-ilm-expiry"], true);
        assert_eq!(wire["objectNamingMode"], "path");
        assert_eq!(wire["caCertPem"], peer.ca_cert_pem().expect("CA field"));
        assert_eq!(wire["futurePeer"]["mode"], "preserved");
        assert_eq!(
            wire["futurePeer"]["accessToken"],
            "OPAQUE-TOKEN-MUST-NOT-PRINT"
        );
        assert_eq!(
            wire["futurePeer"]["nested"][0]["sessionToken"],
            "OPAQUE-TOKEN-MUST-NOT-PRINT"
        );
    }

    #[test]
    fn peer_edit_preserves_omitted_fields_and_exact_future_values() {
        let mut peer: SiteReplicationPeer = serde_json::from_str(
            r#"{
                "endpoint":"https://old.example.test",
                "deploymentID":"deployment-2",
                "sync":"future-sync-mode",
                "defaultbandwidth":{"futureShape":[1,{"safe":true}]},
                "futurePeer":{"nested":[1,2,3]}
            }"#,
        )
        .expect("valid peer fixture");

        peer.set_endpoint("https://new.example.test".into());
        let wire = serde_json::to_value(peer).expect("peer is serializable");

        assert_eq!(wire["endpoint"], "https://new.example.test");
        assert_eq!(wire["sync"], "future-sync-mode");
        assert_eq!(wire["defaultbandwidth"]["futureShape"][1]["safe"], true);
        assert_eq!(wire["futurePeer"]["nested"], serde_json::json!([1, 2, 3]));
        for absent in [
            "name",
            "replicate-ilm-expiry",
            "objectNamingMode",
            "skipTlsVerify",
            "caCertPem",
            "apiVersion",
        ] {
            assert!(wire.get(absent).is_none(), "{absent} must remain omitted");
        }
    }

    #[test]
    fn peer_endpoint_falls_back_to_legacy_string_without_overriding_singular_endpoint() {
        let legacy: SiteReplicationPeer = serde_json::from_value(serde_json::json!({
            "endpoints": "https://legacy.example.test"
        }))
        .expect("legacy peer fixture");
        let both: SiteReplicationPeer = serde_json::from_value(serde_json::json!({
            "endpoint": "https://current.example.test",
            "endpoints": "https://legacy.example.test"
        }))
        .expect("current peer fixture");
        let empty_current: SiteReplicationPeer = serde_json::from_value(serde_json::json!({
            "endpoint": "",
            "endpoints": "https://legacy.example.test"
        }))
        .expect("empty current endpoint fixture");
        let malformed_legacy: SiteReplicationPeer = serde_json::from_value(serde_json::json!({
            "endpoints": ["https://legacy.example.test"]
        }))
        .expect("opaque legacy peer fixture");

        assert_eq!(legacy.endpoint(), Some("https://legacy.example.test"));
        assert_eq!(both.endpoint(), Some("https://current.example.test"));
        assert_eq!(
            empty_current.endpoint(),
            Some("https://legacy.example.test")
        );
        assert_eq!(malformed_legacy.endpoint(), None);
    }

    #[test]
    fn peer_debug_reports_ca_presence_without_exposing_certificate() {
        let info: SiteReplicationInfo =
            serde_json::from_str(SITE_INFO).expect("valid site information fixture");
        let debug = format!("{:?}", info.sites[0]);

        assert!(debug.contains("has_custom_ca: true"));
        assert!(!debug.contains("SAFE-TO-SEND-NOT-SAFE-TO-PRINT"));
        assert!(!debug.contains("https://secondary.example.test"));
        assert!(!debug.contains("sessionToken"));
    }

    #[test]
    fn peer_site_spec_debug_redacts_credentials_and_certificate() {
        let spec = PeerSiteSpec {
            name: "secondary".into(),
            endpoint: "https://secondary.example.test".into(),
            access_key: "ACCESS-SECRET".into(),
            secret_key: "SECRET-SECRET".into(),
            skip_tls_verify: false,
            ca_cert_pem: "CERTIFICATE-SECRET".into(),
        };

        let debug = format!("{spec:?}");
        assert!(!debug.contains("ACCESS-SECRET"));
        assert!(!debug.contains("SECRET-SECRET"));
        assert!(!debug.contains("CERTIFICATE-SECRET"));
        assert!(debug.contains("has_access_key: true"));
        assert!(debug.contains("has_custom_ca: true"));
    }

    #[test]
    fn resolve_peer_prefers_exact_deployment_id_over_name() {
        let mut info: SiteReplicationInfo =
            serde_json::from_str(SITE_INFO).expect("valid site information fixture");
        let mut other = info.sites[0].clone();
        other
            .document
            .insert("name".into(), Value::String("deployment-2".into()));
        other
            .document
            .insert("deploymentID".into(), Value::String("deployment-3".into()));
        info.sites.push(other);

        let resolved = info
            .resolve_peer("deployment-2")
            .expect("deployment ID takes precedence");
        assert_eq!(resolved.deployment_id(), Some("deployment-2"));
        assert_eq!(resolved.name(), Some("secondary"));
    }

    #[test]
    fn resolve_peer_requires_unique_exact_name() {
        let mut info: SiteReplicationInfo =
            serde_json::from_str(SITE_INFO).expect("valid site information fixture");
        let mut duplicate = info.sites[0].clone();
        duplicate
            .document
            .insert("deploymentID".into(), Value::String("deployment-3".into()));
        info.sites.push(duplicate);

        let error = info
            .resolve_peer("secondary")
            .expect_err("duplicate names are ambiguous");
        assert!(matches!(error, crate::Error::Conflict(_)));
    }

    #[test]
    fn resolve_peer_does_not_guess_partial_names() {
        let info: SiteReplicationInfo =
            serde_json::from_str(SITE_INFO).expect("valid site information fixture");

        let error = info
            .resolve_peer("second")
            .expect_err("partial names must not match");
        assert!(matches!(error, crate::Error::NotFound(_)));
    }

    #[test]
    fn edit_status_uses_exact_server_field_names() {
        let status: ReplicateEditStatus = serde_json::from_str(
            r#"{"success":true,"status":"updated","errorDetail":"","apiVersion":"v1","future":"preserved"}"#,
        )
        .expect("valid edit status fixture");

        assert!(status.success);
        assert_eq!(status.error_detail, "");
        assert_eq!(status.extensions["future"], "preserved");
        assert_eq!(
            serde_json::to_value(&status).expect("status is serializable")["errorDetail"],
            ""
        );
        let debug = format!("{status:?}");
        assert!(!debug.contains("updated"));
        assert!(!debug.contains("preserved"));
    }

    #[test]
    fn resync_operations_use_exact_wire_values_and_classify_mutations() {
        for (operation, wire, mutation) in [
            (SiteReplicationResyncOperation::Start, "start", true),
            (SiteReplicationResyncOperation::Status, "status", false),
            (SiteReplicationResyncOperation::Cancel, "cancel", true),
        ] {
            assert_eq!(operation.as_str(), wire);
            assert_eq!(operation.is_mutation(), mutation);
            assert_eq!(
                serde_json::to_value(operation).expect("operation is serializable"),
                wire
            );
            assert_eq!(
                serde_json::from_value::<SiteReplicationResyncOperation>(Value::String(
                    wire.to_string()
                ))
                .expect("operation is deserializable"),
                operation
            );
        }
    }

    #[test]
    fn resync_status_preserves_exact_fields_and_unknown_extensions() {
        let status: SiteReplicationResyncStatus = serde_json::from_value(serde_json::json!({
            "op": "start",
            "id": "resync-id",
            "status": "success",
            "buckets": [{
                "bucket": "photos",
                "status": "started",
                "errorDetail": "",
                "futureBucket": {"attempt": 2}
            }],
            "errorDetail": "",
            "futureResponse": {"revision": 7}
        }))
        .expect("valid site resync status");

        assert_eq!(status.operation, "start");
        assert_eq!(status.resync_id, "resync-id");
        assert_eq!(status.status, "success");
        assert_eq!(status.buckets[0].bucket, "photos");
        assert_eq!(status.buckets[0].status, "started");
        assert_eq!(status.buckets[0].extensions["futureBucket"]["attempt"], 2);
        assert_eq!(status.extensions["futureResponse"]["revision"], 7);

        let wire = serde_json::to_value(status).expect("status is serializable");
        assert_eq!(wire["op"], "start");
        assert_eq!(wire["id"], "resync-id");
        assert_eq!(wire["buckets"][0]["errorDetail"], "");
        assert_eq!(wire["futureResponse"]["revision"], 7);
    }

    #[test]
    fn resync_status_wire_equality_ignores_frozen_output_semantics() {
        let mut status: SiteReplicationResyncStatus = serde_json::from_value(serde_json::json!({
            "op": "status",
            "status": "not-found"
        }))
        .expect("valid site resync status");
        status.capture_semantics();
        status.status = "[REDACTED]".to_string();

        assert!(status.is_not_found());
        assert!(status.has_failure());
        let wire = serde_json::to_vec(&status).expect("status is serializable");
        let round_trip: SiteReplicationResyncStatus =
            serde_json::from_slice(&wire).expect("status is deserializable");
        assert_eq!(status, round_trip);
    }

    #[test]
    fn resync_status_debug_never_prints_arbitrary_server_strings() {
        let status: SiteReplicationResyncStatus = serde_json::from_value(serde_json::json!({
            "op": "SECRET-OPERATION",
            "id": "SECRET-RESYNC-ID",
            "status": "SECRET-STATUS",
            "buckets": [{
                "bucket": "SECRET-BUCKET",
                "status": "SECRET-BUCKET-STATUS",
                "errorDetail": "SECRET-BUCKET-ERROR",
                "SECRET-BUCKET-EXTENSION": "SECRET-BUCKET-VALUE"
            }],
            "errorDetail": "SECRET-ERROR",
            "SECRET-EXTENSION": "SECRET-VALUE"
        }))
        .expect("valid sensitive status fixture");

        let status_debug = format!("{status:?}");
        let bucket_debug = format!("{:?}", status.buckets[0]);
        for secret in [
            "SECRET-OPERATION",
            "SECRET-RESYNC-ID",
            "SECRET-STATUS",
            "SECRET-BUCKET",
            "SECRET-BUCKET-STATUS",
            "SECRET-BUCKET-ERROR",
            "SECRET-BUCKET-EXTENSION",
            "SECRET-BUCKET-VALUE",
            "SECRET-ERROR",
            "SECRET-EXTENSION",
            "SECRET-VALUE",
        ] {
            assert!(!status_debug.contains(secret));
            assert!(!bucket_debug.contains(secret));
        }
        assert!(status_debug.contains("bucket_count: 1"));
        assert!(bucket_debug.contains("has_error_detail: true"));
    }

    #[test]
    fn resync_status_detects_overall_and_partial_bucket_failures() {
        let success_bucket = SiteReplicationResyncBucketStatus {
            bucket: "photos".into(),
            status: "started".into(),
            ..Default::default()
        };
        let success = SiteReplicationResyncStatus {
            status: "success".into(),
            buckets: vec![success_bucket.clone()],
            ..Default::default()
        };
        assert!(!success.has_failure());

        let overall_failed = SiteReplicationResyncStatus {
            status: "FAILED".into(),
            ..Default::default()
        };
        assert!(overall_failed.has_failure());

        let partial_overall = SiteReplicationResyncStatus {
            status: "partial".into(),
            ..Default::default()
        };
        assert!(partial_overall.has_failure());

        let overall_error = SiteReplicationResyncStatus {
            status: "success".into(),
            error_detail: "partial failure".into(),
            ..Default::default()
        };
        assert!(overall_error.has_failure());

        let failed_bucket = SiteReplicationResyncStatus {
            status: "success".into(),
            buckets: vec![SiteReplicationResyncBucketStatus {
                status: "failed".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(failed_bucket.has_failure());

        let bucket_error = SiteReplicationResyncStatus {
            status: "success".into(),
            buckets: vec![SiteReplicationResyncBucketStatus {
                status: "started".into(),
                error_detail: "target rejected bucket".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(bucket_error.has_failure());
        assert!(bucket_error.buckets[0].has_failure());
        assert!(!success_bucket.has_failure());
    }

    #[test]
    fn ca_bundle_requires_valid_x509_der() {
        assert!(validate_site_replication_ca_bundle(include_bytes!("test_ca.pem")).is_ok());
        assert!(
            validate_site_replication_ca_bundle(
                b"-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n"
            )
            .is_err()
        );
    }

    #[test]
    fn ca_bundle_accepts_exact_limit_and_rejects_limit_plus_one() {
        let certificate = include_bytes!("test_ca.pem");
        let mut exact = certificate.to_vec();
        exact.push(b'\n');
        exact.resize(MAX_SITE_REPLICATION_CA_CERT_BYTES, b' ');
        assert_eq!(exact.len(), MAX_SITE_REPLICATION_CA_CERT_BYTES);
        assert!(validate_site_replication_ca_bundle(&exact).is_ok());

        exact.push(b' ');
        assert!(validate_site_replication_ca_bundle(&exact).is_err());
    }

    #[test]
    fn repair_identifiers_are_strictly_validated() {
        assert!(
            validate_site_replication_repair_token("abcdefghijklmnopqrstuvwxyzABCDEFGH012345678")
                .is_ok()
        );
        assert!(validate_site_replication_repair_token("short").is_err());
        assert!(
            validate_site_replication_repair_token("abcdefghijklmnopqrstuvwxyzABCDEFGH01234567=")
                .is_err()
        );

        assert!(
            validate_site_replication_repair_operation_id("550e8400-e29b-41d4-a716-446655440000")
                .is_ok()
        );
        assert!(
            validate_site_replication_repair_operation_id("550e8400e29b41d4a716446655440000")
                .is_err()
        );

        let preflight: SiteReplicationRepairPreflight = serde_json::from_value(serde_json::json!({
            "mode": "dry-run",
            "status": "planned",
            "preflightToken": "abcdefghijklmnopqrstuvwxyzABCDEFGH012345678",
            "retryEvents": 0,
            "sites": {}
        }))
        .expect("valid empty preflight");
        assert!(!format!("{preflight:?}").contains(&preflight.preflight_token));
    }

    #[test]
    fn repair_snapshot_rejects_inconsistent_counts_and_detects_partial() {
        let json = serde_json::json!({
            "mode": "execute",
            "operationId": "550e8400-e29b-41d4-a716-446655440000",
            "status": "partial",
            "createdAt": "2026-07-25T00:00:00Z",
            "updatedAt": "2026-07-25T00:01:00Z",
            "sites": {
                "dep-2": {
                    "deploymentId": "dep-2",
                    "name": "secondary",
                    "families": {
                        "iam": {
                            "planned": 1,
                            "succeeded": 0,
                            "failed": 1,
                            "retryEvents": 1,
                            "tasks": [{
                                "taskId": "abcdefghijklmnopqrstuvwxyzABCDEFGH012345678",
                                "status": "failed",
                                "error": "remote-operation-failed"
                            }],
                            "errors": ["remote-operation-failed"]
                        }
                    }
                }
            }
        });
        let status: SiteReplicationRepairOperationStatus =
            serde_json::from_value(json.clone()).expect("valid repair response");
        assert!(
            status
                .validate("550e8400-e29b-41d4-a716-446655440000")
                .is_ok()
        );
        assert!(status.has_failure());

        let mut invalid = json;
        invalid["sites"]["dep-2"]["families"]["iam"]["planned"] = Value::from(0);
        let status: SiteReplicationRepairOperationStatus =
            serde_json::from_value(invalid).expect("syntactically valid repair response");
        assert!(
            status
                .validate("550e8400-e29b-41d4-a716-446655440000")
                .is_err()
        );
    }
}
