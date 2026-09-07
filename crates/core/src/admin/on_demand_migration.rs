//! On-Demand Migration administration, independent of HTTP transport.
//!
//! A bucket names an external S3-compatible source bucket. A GET that misses
//! locally is served from the source and stored locally, and a background
//! backfill job pulls the remainder. The wire contract is pinned by the
//! vendored fixtures in `tests/fixtures/on_demand_migration/`.
//!
//! Two shapes live here on purpose. [`OnDemandMigrationConfigRequest`] is what
//! `rc` sends: it carries the plaintext secret in zeroizing storage and is
//! serialized once, into a zeroizing buffer. [`OnDemandMigrationConfigView`] is
//! what the server returns: every field is optional with the server's own
//! default, so an older server that omits a field still parses, and the
//! credential values are dropped during deserialization so nothing downstream
//! can print them.

use crate::{Error, Result};
use async_trait::async_trait;
use serde::de::Deserializer;
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use zeroize::{Zeroize, Zeroizing};

/// Capability label used in unsupported-feature diagnostics.
pub const ON_DEMAND_MIGRATION_CAPABILITY: &str = "admin.on-demand-migration";

/// Upper bound for one admin response body. A status document with latency
/// histograms is a few kilobytes; a megabyte is generous without being unbounded.
pub const MAX_ON_DEMAND_MIGRATION_RESPONSE_BYTES: usize = 1024 * 1024;

/// Upper bound for a CA bundle passed with `--ca-cert`.
pub const MAX_ON_DEMAND_MIGRATION_CA_CERT_BYTES: usize = 64 * 1024;

/// The placeholder the server substitutes for a credential in every response.
pub const REDACTED_SECRET: &str = "REDACTED";

/// Largest value the server accepts for `policy.inline_max_bytes` (256 MiB).
pub const MAX_INLINE_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Largest value the server accepts for `policy.max_concurrent_pulls`.
pub const MAX_CONCURRENT_PULLS: u32 = 256;

// ---------------------------------------------------------------------------
// Enumerations shared by the request and the view
// ---------------------------------------------------------------------------

/// Source vendor family for the S3-speaking providers `rc` can configure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceProvider {
    /// Generic S3-compatible endpoint.
    S3,
    Aws,
    Minio,
    Rustfs,
    R2,
    /// GCS XML interoperability API with HMAC keys.
    Gcs,
}

impl SourceProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::Aws => "aws",
            Self::Minio => "minio",
            Self::Rustfs => "rustfs",
            Self::R2 => "r2",
            Self::Gcs => "gcs",
        }
    }

    /// Only AWS derives its endpoint from the region.
    pub const fn requires_endpoint(self) -> bool {
        !matches!(self, Self::Aws)
    }
}

/// Bucket addressing style; `auto` is resolved by the server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathStyle {
    #[default]
    Auto,
    Path,
    Virtual,
}

impl PathStyle {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Path => "path",
            Self::Virtual => "virtual",
        }
    }
}

/// What a HEAD that misses locally does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadPolicy {
    #[default]
    Proxy,
    LocalOnly,
}

impl HeadPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::LocalOnly => "local_only",
        }
    }
}

/// Whether a Range GET also queues a whole-object background pull.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RangeGetPolicy {
    #[default]
    ServeAndBackfill,
    ServeOnly,
}

impl RangeGetPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ServeAndBackfill => "serve_and_backfill",
            Self::ServeOnly => "serve_only",
        }
    }
}

/// How a source failure is answered to the client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceErrorPolicy {
    #[default]
    Propagate,
    NotFound,
}

impl SourceErrorPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Propagate => "propagate",
            Self::NotFound => "not_found",
        }
    }
}

/// When the backfill job treats a local object as already migrated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipExisting {
    #[default]
    Always,
    EtagOrSize,
}

impl SkipExisting {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::EtagOrSize => "etag_or_size",
        }
    }
}

// ---------------------------------------------------------------------------
// Request shape
// ---------------------------------------------------------------------------

/// Static credentials for the source, with the secret in zeroizing storage.
///
/// `Debug` never prints the secret or the session token.
#[derive(Clone)]
pub struct SourceCredentialsRequest {
    pub access_key: String,
    pub secret_key: Zeroizing<String>,
    pub session_token: Option<Zeroizing<String>>,
}

impl std::fmt::Debug for SourceCredentialsRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SourceCredentialsRequest")
            .field("access_key", &self.access_key)
            .field("secret_key", &REDACTED_SECRET)
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| REDACTED_SECRET),
            )
            .finish()
    }
}

impl Serialize for SourceCredentialsRequest {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("SourceCredentials", 3)?;
        state.serialize_field("access_key", &self.access_key)?;
        state.serialize_field("secret_key", self.secret_key.as_str())?;
        state.serialize_field(
            "session_token",
            &self.session_token.as_deref().map(String::as_str),
        )?;
        state.end()
    }
}

/// TLS settings for the source connection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TlsRequest {
    pub skip_verify: bool,
    pub ca_cert_pem: Option<String>,
}

/// The external source bucket.
#[derive(Clone, Debug, Serialize)]
pub struct SourceRequest {
    pub provider: SourceProvider,
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    pub path_style: PathStyle,
    /// `None` means anonymous access to a public source bucket.
    pub credentials: Option<SourceCredentialsRequest>,
    pub tls: TlsRequest,
}

/// Key filters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct FilterRequest {
    /// Only local keys with this prefix consult the source.
    pub prefix: Option<String>,
    /// Prepended to the local key to form the source key.
    pub source_prefix: Option<String>,
}

/// Per-request source timeouts, in milliseconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceTimeout {
    #[serde(default = "default_connect_ms")]
    pub connect_ms: u64,
    #[serde(default = "default_first_byte_ms")]
    pub first_byte_ms: u64,
    #[serde(default = "default_idle_ms")]
    pub idle_ms: u64,
}

impl Default for SourceTimeout {
    fn default() -> Self {
        Self {
            connect_ms: default_connect_ms(),
            first_byte_ms: default_first_byte_ms(),
            idle_ms: default_idle_ms(),
        }
    }
}

/// Read-path policy.
///
/// The request sends every field explicitly, filled with the server defaults
/// the fixtures pin, so the body `rc` produces is the documented wire shape
/// rather than a partial document whose meaning depends on server defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyConfig {
    #[serde(default)]
    pub head: HeadPolicy,
    #[serde(default)]
    pub range_get: RangeGetPolicy,
    #[serde(default)]
    pub source_error: SourceErrorPolicy,
    #[serde(default)]
    pub list_through: bool,
    #[serde(default = "default_true")]
    pub respect_local_delete_marker: bool,
    #[serde(default = "default_true")]
    pub preserve_etag: bool,
    #[serde(default)]
    pub copy_tags: bool,
    #[serde(default = "default_true")]
    pub emit_events: bool,
    #[serde(default = "default_negative_cache_ttl_secs")]
    pub negative_cache_ttl_secs: u64,
    #[serde(default = "default_inline_max_bytes")]
    pub inline_max_bytes: u64,
    #[serde(default = "default_multipart_part_size_bytes")]
    pub multipart_part_size_bytes: u64,
    #[serde(default = "default_max_concurrent_pulls")]
    pub max_concurrent_pulls: u32,
    #[serde(default = "default_pull_queue_capacity")]
    pub pull_queue_capacity: u32,
    #[serde(default)]
    pub source_timeout: SourceTimeout,
    #[serde(default)]
    pub bandwidth_limit_bytes_per_sec: Option<u64>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            head: HeadPolicy::default(),
            range_get: RangeGetPolicy::default(),
            source_error: SourceErrorPolicy::default(),
            list_through: false,
            respect_local_delete_marker: true,
            preserve_etag: true,
            copy_tags: false,
            emit_events: true,
            negative_cache_ttl_secs: default_negative_cache_ttl_secs(),
            inline_max_bytes: default_inline_max_bytes(),
            multipart_part_size_bytes: default_multipart_part_size_bytes(),
            max_concurrent_pulls: default_max_concurrent_pulls(),
            pull_queue_capacity: default_pull_queue_capacity(),
            source_timeout: SourceTimeout::default(),
            bandwidth_limit_bytes_per_sec: None,
        }
    }
}

const fn default_true() -> bool {
    true
}
const fn default_version() -> u32 {
    1
}
const fn default_negative_cache_ttl_secs() -> u64 {
    30
}
const fn default_inline_max_bytes() -> u64 {
    16 * 1024 * 1024
}
const fn default_multipart_part_size_bytes() -> u64 {
    64 * 1024 * 1024
}
const fn default_max_concurrent_pulls() -> u32 {
    8
}
const fn default_pull_queue_capacity() -> u32 {
    1024
}
const fn default_connect_ms() -> u64 {
    5000
}
const fn default_first_byte_ms() -> u64 {
    15_000
}
const fn default_idle_ms() -> u64 {
    30_000
}

/// The document `PUT .../on-demand-migration/{bucket}` accepts.
#[derive(Clone, Debug, Serialize)]
pub struct OnDemandMigrationConfigRequest {
    pub version: u32,
    pub enabled: bool,
    pub source: SourceRequest,
    pub filter: FilterRequest,
    pub policy: PolicyConfig,
}

impl OnDemandMigrationConfigRequest {
    /// A version-1, enabled configuration with default filter and policy.
    pub fn new(source: SourceRequest) -> Self {
        Self {
            version: default_version(),
            enabled: true,
            source,
            filter: FilterRequest::default(),
            policy: PolicyConfig::default(),
        }
    }

    /// Reject locally what the server would reject, before any network access
    /// and before a secret is read. Every failure is a usage error.
    pub fn validate(&self) -> Result<()> {
        let source = &self.source;
        match source.endpoint.as_deref() {
            Some(endpoint) => validate_endpoint(endpoint)?,
            None if source.provider.requires_endpoint() => {
                return Err(Error::Config(format!(
                    "--endpoint is required for provider {}",
                    source.provider.as_str()
                )));
            }
            None => {}
        }
        if source.region.trim().is_empty() {
            return Err(Error::Config("--region must not be empty".into()));
        }
        validate_source_bucket(&source.bucket)?;
        if let Some(credentials) = &source.credentials {
            if credentials.access_key.is_empty() {
                return Err(Error::Config("--access-key must not be empty".into()));
            }
            if credentials.secret_key.is_empty() {
                return Err(Error::Config(
                    "The source secret key must not be empty".into(),
                ));
            }
            if credentials
                .session_token
                .as_ref()
                .is_some_and(|token| token.is_empty())
            {
                return Err(Error::Config(
                    "The source session token must not be empty".into(),
                ));
            }
        }
        if let Some(pem) = source.tls.ca_cert_pem.as_deref() {
            validate_ca_cert_pem(pem)?;
        }
        for (flag, value) in [
            ("--prefix", &self.filter.prefix),
            ("--source-prefix", &self.filter.source_prefix),
        ] {
            if value.as_deref().is_some_and(str::is_empty) {
                return Err(Error::Config(format!("{flag} must not be empty")));
            }
        }
        let policy = &self.policy;
        if policy.inline_max_bytes > MAX_INLINE_MAX_BYTES {
            return Err(Error::Config(format!(
                "--inline-max-bytes must be at most {MAX_INLINE_MAX_BYTES}"
            )));
        }
        if !(1..=MAX_CONCURRENT_PULLS).contains(&policy.max_concurrent_pulls) {
            return Err(Error::Config(format!(
                "--max-concurrent-pulls must be between 1 and {MAX_CONCURRENT_PULLS}"
            )));
        }
        Ok(())
    }

    /// Serialize into a zeroizing buffer. The result is the only copy of the
    /// plaintext body; callers hand it to the transport without cloning.
    pub fn to_wire_json(&self) -> Result<Zeroizing<Vec<u8>>> {
        self.validate()?;
        Ok(Zeroizing::new(serde_json::to_vec(self)?))
    }
}

/// `http(s)://host[:port]` with nothing else: no path, query, fragment or
/// userinfo. Userinfo would smuggle a credential into a log line.
fn validate_endpoint(endpoint: &str) -> Result<()> {
    let parsed = url::Url::parse(endpoint)
        .map_err(|_| Error::Config("--endpoint must be an http(s) URL".into()))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Error::Config("--endpoint must use http or https".into()));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(Error::Config("--endpoint must name a host".into()));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::Config(
            "--endpoint must not embed credentials; pass them with --access-key".into(),
        ));
    }
    if !matches!(parsed.path(), "" | "/") || parsed.query().is_some() || parsed.fragment().is_some()
    {
        return Err(Error::Config(
            "--endpoint must be scheme://host[:port] with no path, query or fragment".into(),
        ));
    }
    Ok(())
}

fn validate_source_bucket(bucket: &str) -> Result<()> {
    if bucket.is_empty() || bucket.contains('/') || bucket.chars().any(char::is_whitespace) {
        return Err(Error::Config(
            "--source-bucket must be a non-empty bucket name without '/' or whitespace".into(),
        ));
    }
    Ok(())
}

/// The server requires a PEM certificate block; checking here turns a wrong
/// file into a usage error before the secret is prompted for.
pub fn validate_ca_cert_pem(pem: &str) -> Result<()> {
    if pem.len() > MAX_ON_DEMAND_MIGRATION_CA_CERT_BYTES {
        return Err(Error::Config(format!(
            "--ca-cert exceeds {MAX_ON_DEMAND_MIGRATION_CA_CERT_BYTES} bytes"
        )));
    }
    if !pem.contains("-----BEGIN CERTIFICATE-----") {
        return Err(Error::Config(
            "--ca-cert must contain a PEM certificate (-----BEGIN CERTIFICATE-----)".into(),
        ));
    }
    Ok(())
}

/// A local bucket name as it appears in the admin route. Anything that could
/// change the route (a slash, a dot segment, whitespace) is refused here so the
/// transport never has to reason about it.
pub fn validate_local_bucket(bucket: &str) -> Result<()> {
    if bucket.is_empty()
        || bucket.len() > 255
        || matches!(bucket, "." | "..")
        || bucket
            .chars()
            .any(|c| c == '/' || c == '\\' || c == '%' || c == '?' || c == '#' || c.is_whitespace())
    {
        return Err(Error::InvalidPath(
            "Expected alias/bucket with a plain bucket name".into(),
        ));
    }
    Ok(())
}

/// Body of `POST .../backfill?op=start`; every field is optional.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BackfillStartRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_existing: Option<SkipExisting>,
    /// List and count only; nothing is queued.
    pub dry_run: bool,
}

impl BackfillStartRequest {
    pub fn validate(&self) -> Result<()> {
        if self.prefix.as_deref().is_some_and(str::is_empty) {
            return Err(Error::Config("--prefix must not be empty".into()));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

/// Redacted credential summary. Only presence survives deserialization: the
/// server substitutes `REDACTED`, and a server that did not must still never
/// reach stdout, so the values are discarded at the parsing boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceCredentialsView {
    pub access_key: String,
    pub has_secret_key: bool,
    pub has_session_token: bool,
}

impl<'de> Deserialize<'de> for SourceCredentialsView {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            access_key: String,
            #[serde(default)]
            secret_key: Option<String>,
            #[serde(default)]
            session_token: Option<String>,
        }
        let wire = Wire::deserialize(deserializer)?;
        // Wipe whatever the server sent before it goes out of scope.
        let mut secret_key = Zeroizing::new(wire.secret_key.unwrap_or_default());
        let mut session_token = Zeroizing::new(wire.session_token.unwrap_or_default());
        let view = Self {
            access_key: wire.access_key,
            has_secret_key: !secret_key.is_empty(),
            has_session_token: !session_token.is_empty(),
        };
        secret_key.zeroize();
        session_token.zeroize();
        Ok(view)
    }
}

impl Serialize for SourceCredentialsView {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("SourceCredentials", 3)?;
        state.serialize_field("access_key", &self.access_key)?;
        state.serialize_field(
            "secret_key",
            &self.has_secret_key.then_some(REDACTED_SECRET),
        )?;
        state.serialize_field(
            "session_token",
            &self.has_session_token.then_some(REDACTED_SECRET),
        )?;
        state.end()
    }
}

/// TLS settings as returned by the server. The CA bundle is public material,
/// but it is long; only its presence is kept for display.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TlsView {
    pub skip_verify: bool,
    pub has_ca_cert: bool,
}

impl<'de> Deserialize<'de> for TlsView {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            skip_verify: bool,
            #[serde(default)]
            ca_cert_pem: Option<String>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            skip_verify: wire.skip_verify,
            has_ca_cert: wire.ca_cert_pem.is_some_and(|pem| !pem.is_empty()),
        })
    }
}

impl Serialize for TlsView {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("Tls", 2)?;
        state.serialize_field("skip_verify", &self.skip_verify)?;
        state.serialize_field("has_ca_cert", &self.has_ca_cert)?;
        state.end()
    }
}

/// The source as returned by the server. `provider` stays a string so a
/// provider this build does not know (for example `azure`) still displays.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceView {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub path_style: PathStyle,
    #[serde(default)]
    pub credentials: Option<SourceCredentialsView>,
    #[serde(default)]
    pub tls: TlsView,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterView {
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub source_prefix: Option<String>,
}

/// The redacted configuration document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnDemandMigrationConfigView {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub source: SourceView,
    #[serde(default)]
    pub filter: FilterView,
    #[serde(default)]
    pub policy: PolicyConfig,
}

impl Default for OnDemandMigrationConfigView {
    fn default() -> Self {
        Self {
            version: default_version(),
            enabled: true,
            source: SourceView::default(),
            filter: FilterView::default(),
            policy: PolicyConfig::default(),
        }
    }
}

/// What the `PUT` probe learned about the source.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeSummary {
    #[serde(default)]
    pub reachable: bool,
    #[serde(default)]
    pub listable: bool,
    #[serde(default)]
    pub sample_key: Option<String>,
}

/// Response of `PUT .../on-demand-migration/{bucket}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnDemandMigrationSetResult {
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub config: Option<OnDemandMigrationConfigView>,
    /// `None` for a dry run, which saves nothing.
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub probe: Option<ProbeSummary>,
}

/// Response of `GET .../on-demand-migration/{bucket}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnDemandMigrationConfigResult {
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub config: Option<OnDemandMigrationConfigView>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakerStatus {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub opened_at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyBucket {
    #[serde(default)]
    pub le_ms: u64,
    #[serde(default)]
    pub count: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLatency {
    #[serde(default)]
    pub buckets: Vec<LatencyBucket>,
    #[serde(default)]
    pub count: u64,
    #[serde(default)]
    pub sum_ms: u64,
}

/// Per-node runtime counters. The nested maps keep the outcome and path
/// labels open-ended so a new label on the server displays instead of failing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCounters {
    /// Operation (`get`, `head`) to outcome (`source_hit`, `source_miss`, ...) to count.
    #[serde(default)]
    pub requests_total: BTreeMap<String, BTreeMap<String, u64>>,
    #[serde(default)]
    pub pulled_bytes_total: u64,
    /// Pull path (`inline`, `background`, `backfill`) to count.
    #[serde(default)]
    pub pulled_objects_total: BTreeMap<String, u64>,
    /// Failure class to count.
    #[serde(default)]
    pub pull_failures_total: BTreeMap<String, u64>,
    #[serde(default)]
    pub source_latency: Option<SourceLatency>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastSourceError {
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub at: Option<String>,
}

/// Counters of the bucket's backfill job as embedded in the status document.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillSummary {
    #[serde(default)]
    pub job_id: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub listed: u64,
    #[serde(default)]
    pub enqueued: u64,
    #[serde(default)]
    pub pulled: u64,
    #[serde(default)]
    pub skipped_existing: u64,
    #[serde(default)]
    pub failed: u64,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Response of `GET .../on-demand-migration/{bucket}/status`.
///
/// This is the answering node's view: counters, queue depth and breaker state
/// are per node, while the configuration is cluster-wide.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OnDemandMigrationStatus {
    #[serde(default)]
    pub configured: bool,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub module_enabled: bool,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub endpoint_host: Option<String>,
    #[serde(default)]
    pub breaker: Option<BreakerStatus>,
    #[serde(default)]
    pub counters: Option<RuntimeCounters>,
    #[serde(default)]
    pub last_source_error: Option<LastSourceError>,
    #[serde(default)]
    pub inflight_pulls: u64,
    #[serde(default)]
    pub queue_depth: u64,
    /// Deliberately `null` on the server today. Rendered as an em dash, never
    /// as zero: a missing ratio and a zero ratio mean different things.
    #[serde(default)]
    pub served_by_source_ratio: Option<f64>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub backfill: Option<BackfillSummary>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillLastError {
    #[serde(default)]
    pub class: String,
    /// Hash of the failing key; the key itself never leaves the server.
    #[serde(default)]
    pub key_hash: Option<String>,
    #[serde(default)]
    pub at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillOwner {
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub lease_until: Option<String>,
}

/// The backfill checkpoint document as stored on the server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillJob {
    #[serde(default = "default_version")]
    pub format_version: u32,
    #[serde(default)]
    pub job_id: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub config_updated_at: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub skip_existing: SkipExisting,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub listed: u64,
    #[serde(default)]
    pub enqueued: u64,
    #[serde(default)]
    pub pulled: u64,
    #[serde(default)]
    pub skipped_existing: u64,
    #[serde(default)]
    pub failed: u64,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub last_key: Option<String>,
    #[serde(default)]
    pub last_error: Option<BackfillLastError>,
    #[serde(default)]
    pub failed_keys: Vec<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub owner: Option<BackfillOwner>,
}

impl BackfillJob {
    /// Whether the job can still change. `--watch` stops on a terminal state;
    /// an unknown state from a newer server is treated as still running so the
    /// watcher keeps refreshing rather than declaring victory early.
    pub fn is_terminal(&self) -> bool {
        is_terminal_backfill_state(&self.state)
    }
}

pub fn is_terminal_backfill_state(state: &str) -> bool {
    matches!(
        state,
        "cancelled" | "completed" | "completed_with_failures" | "failed"
    )
}

/// Response of `POST`/`GET .../on-demand-migration/{bucket}/backfill`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillJobResult {
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub job: Option<BackfillJob>,
}

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------

/// Administrative operations for on-demand migration.
///
/// Writes are never automatically retried: a `PUT` probes the source and a
/// backfill start takes a lease, so a repeated request is a second decision.
#[async_trait]
pub trait OnDemandMigrationApi: Send + Sync {
    /// Validate, probe and (unless `dry_run`) save the configuration.
    async fn set_on_demand_migration(
        &self,
        bucket: &str,
        config: &OnDemandMigrationConfigRequest,
        dry_run: bool,
    ) -> Result<OnDemandMigrationSetResult>;

    async fn get_on_demand_migration(&self, bucket: &str) -> Result<OnDemandMigrationConfigResult>;

    /// Idempotent; already-pulled objects stay in place.
    async fn delete_on_demand_migration(&self, bucket: &str) -> Result<()>;

    async fn on_demand_migration_status(&self, bucket: &str) -> Result<OnDemandMigrationStatus>;

    async fn start_on_demand_migration_backfill(
        &self,
        bucket: &str,
        request: &BackfillStartRequest,
    ) -> Result<BackfillJobResult>;

    async fn cancel_on_demand_migration_backfill(&self, bucket: &str) -> Result<BackfillJobResult>;

    async fn on_demand_migration_backfill_status(&self, bucket: &str) -> Result<BackfillJobResult>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const SET_REQUEST: &str =
        include_str!("../../tests/fixtures/on_demand_migration/set_request.json");
    const SET_RESPONSE: &str =
        include_str!("../../tests/fixtures/on_demand_migration/set_response.json");
    const GET_RESPONSE: &str =
        include_str!("../../tests/fixtures/on_demand_migration/get_response.json");
    const STATUS: &str = include_str!("../../tests/fixtures/on_demand_migration/status.json");
    const STATUS_WITH_BACKFILL: &str =
        include_str!("../../tests/fixtures/on_demand_migration/status_with_backfill.json");
    const BACKFILL_JOB: &str =
        include_str!("../../tests/fixtures/on_demand_migration/backfill_job.json");

    fn fixture_request() -> OnDemandMigrationConfigRequest {
        let mut request = OnDemandMigrationConfigRequest::new(SourceRequest {
            provider: SourceProvider::Minio,
            endpoint: Some("https://source.example.com:9000".into()),
            region: "us-east-1".into(),
            bucket: "legacy-photos".into(),
            path_style: PathStyle::Auto,
            credentials: Some(SourceCredentialsRequest {
                access_key: "AKIASOURCE".into(),
                secret_key: Zeroizing::new("sourceSecretKey123".into()),
                session_token: None,
            }),
            tls: TlsRequest::default(),
        });
        request.filter.source_prefix = Some("photos/".into());
        request
    }

    #[test]
    fn set_request_matches_the_plaintext_wire_fixture() {
        let body = fixture_request().to_wire_json().unwrap();
        let actual: Value = serde_json::from_slice(&body).unwrap();
        let expected: Value = serde_json::from_str(SET_REQUEST).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn request_debug_never_prints_the_secret() {
        let request = fixture_request();
        let debug = format!("{request:?}");
        assert!(debug.contains("AKIASOURCE"));
        assert!(!debug.contains("sourceSecretKey123"));
        assert!(debug.contains(REDACTED_SECRET));
    }

    #[test]
    fn set_response_parses_and_drops_the_redacted_secret() {
        let result: OnDemandMigrationSetResult = serde_json::from_str(SET_RESPONSE).unwrap();
        assert_eq!(result.bucket, "photos");
        assert!(!result.dry_run);
        assert_eq!(result.updated_at.as_deref(), Some("2026-09-02T10:00:00Z"));
        let probe = result.probe.unwrap();
        assert!(probe.reachable && probe.listable);
        assert_eq!(probe.sample_key.as_deref(), Some("photos/2024/01.jpg"));
        let config = result.config.unwrap();
        assert_eq!(config.source.provider, "minio");
        let credentials = config.source.credentials.unwrap();
        assert_eq!(credentials.access_key, "AKIASOURCE");
        assert!(credentials.has_secret_key);
        assert!(!credentials.has_session_token);
        let serialized = serde_json::to_value(&credentials).unwrap();
        assert_eq!(serialized["secret_key"], REDACTED_SECRET);
        assert_eq!(serialized["session_token"], Value::Null);
    }

    #[test]
    fn get_response_matches_the_fixture_and_redacts_a_leaked_secret() {
        let result: OnDemandMigrationConfigResult = serde_json::from_str(GET_RESPONSE).unwrap();
        assert_eq!(result.bucket, "photos");
        let config = result.config.unwrap();
        assert!(config.enabled);
        assert_eq!(config.version, 1);
        assert_eq!(config.filter.source_prefix.as_deref(), Some("photos/"));
        assert_eq!(config.policy, PolicyConfig::default());
        assert_eq!(config.source.path_style, PathStyle::Auto);

        // A server that failed to redact must not make it to output either.
        let leaked = GET_RESPONSE.replace("\"REDACTED\"", "\"plaintext-secret\"");
        let result: OnDemandMigrationConfigResult = serde_json::from_str(&leaked).unwrap();
        let text = serde_json::to_string(&result).unwrap();
        assert!(!text.contains("plaintext-secret"));
        assert!(text.contains(REDACTED_SECRET));
    }

    #[test]
    fn status_fixture_keeps_the_null_ratio_and_counters() {
        let status: OnDemandMigrationStatus = serde_json::from_str(STATUS).unwrap();
        assert!(status.configured && status.enabled && status.module_enabled);
        assert_eq!(status.provider.as_deref(), Some("minio"));
        assert_eq!(status.endpoint_host.as_deref(), Some("source.example.com"));
        assert_eq!(status.breaker.as_ref().unwrap().state, "half_open");
        assert_eq!(status.served_by_source_ratio, None);
        assert_eq!(status.inflight_pulls, 1);
        assert_eq!(status.queue_depth, 1);
        assert!(status.backfill.is_none());
        let counters = status.counters.unwrap();
        assert_eq!(counters.pulled_bytes_total, 4096);
        assert_eq!(counters.requests_total["get"]["source_hit"], 2);
        assert_eq!(counters.pull_failures_total["source_timeout"], 1);
        assert_eq!(counters.source_latency.unwrap().count, 3);
        assert_eq!(status.last_source_error.unwrap().class, "server_error");
    }

    #[test]
    fn status_with_backfill_fixture_carries_the_summary() {
        let status: OnDemandMigrationStatus = serde_json::from_str(STATUS_WITH_BACKFILL).unwrap();
        let backfill = status.backfill.unwrap();
        assert_eq!(backfill.job_id, "11111111-1111-4111-8111-111111111111");
        assert_eq!(backfill.state, "running");
        assert_eq!(backfill.pulled, 1400);
        assert_eq!(backfill.bytes, 73_400_320);
    }

    #[test]
    fn backfill_job_fixture_parses_and_is_not_terminal() {
        let result: BackfillJobResult = serde_json::from_str(BACKFILL_JOB).unwrap();
        assert_eq!(result.bucket, "photos");
        let job = result.job.unwrap();
        assert_eq!(job.state, "running");
        assert!(!job.is_terminal());
        assert_eq!(job.skip_existing, SkipExisting::Always);
        assert_eq!(job.prefix.as_deref(), Some("photos/"));
        assert_eq!(job.last_error.unwrap().class, "source_timeout");
        assert_eq!(job.owner.unwrap().node, "node-a:9000");
        assert_eq!(job.failed_keys, vec!["9f2c3b0a1d4e5f60"]);
        for state in [
            "cancelled",
            "completed",
            "completed_with_failures",
            "failed",
        ] {
            assert!(is_terminal_backfill_state(state), "{state}");
        }
        for state in ["pending", "running", "paused", "something_new"] {
            assert!(!is_terminal_backfill_state(state), "{state}");
        }
    }

    #[test]
    fn older_servers_that_omit_fields_still_parse() {
        let status: OnDemandMigrationStatus = serde_json::from_str("{}").unwrap();
        assert!(!status.configured);
        assert_eq!(status.served_by_source_ratio, None);
        let config: OnDemandMigrationConfigResult =
            serde_json::from_str(r#"{"bucket":"b","config":{"source":{"provider":"s3"}}}"#)
                .unwrap();
        let config = config.config.unwrap();
        assert!(config.enabled);
        assert_eq!(config.policy.max_concurrent_pulls, 8);
        assert!(config.source.credentials.is_none());
        let job: BackfillJobResult = serde_json::from_str(r#"{"bucket":"b"}"#).unwrap();
        assert!(job.job.is_none());
        // Unknown fields from a newer server are ignored rather than fatal.
        let newer: OnDemandMigrationStatus =
            serde_json::from_value(json!({"configured": true, "future_field": 1})).unwrap();
        assert!(newer.configured);
    }

    #[test]
    fn validation_rejects_what_the_server_would() {
        let mut request = fixture_request();
        request.source.endpoint = None;
        assert!(request.validate().is_err());
        request.source.provider = SourceProvider::Aws;
        assert!(request.validate().is_ok());

        for endpoint in [
            "source.example.com",
            "ftp://source.example.com",
            "https://user:pw@source.example.com",
            "https://source.example.com/path",
            "https://source.example.com/?x=1",
            "https://source.example.com/#frag",
        ] {
            let mut request = fixture_request();
            request.source.endpoint = Some(endpoint.into());
            assert!(request.validate().is_err(), "{endpoint}");
        }
        let mut request = fixture_request();
        request.source.endpoint = Some("http://127.0.0.1:9000/".into());
        assert!(request.validate().is_ok());

        let mut request = fixture_request();
        request.source.region = " ".into();
        assert!(request.validate().is_err());
        let mut request = fixture_request();
        request.source.bucket = "a/b".into();
        assert!(request.validate().is_err());
        let mut request = fixture_request();
        request.filter.prefix = Some(String::new());
        assert!(request.validate().is_err());
        let mut request = fixture_request();
        request.policy.inline_max_bytes = MAX_INLINE_MAX_BYTES + 1;
        assert!(request.validate().is_err());
        let mut request = fixture_request();
        request.policy.max_concurrent_pulls = 0;
        assert!(request.validate().is_err());
        let mut request = fixture_request();
        request.source.tls.ca_cert_pem = Some("not a certificate".into());
        assert!(request.validate().is_err());
        let mut request = fixture_request();
        request.source.credentials = None;
        assert!(request.validate().is_ok());
        assert_eq!(
            fixture_request().validate().map_err(|e| e.exit_code()),
            Ok(())
        );
        let mut request = fixture_request();
        request.source.endpoint = None;
        assert_eq!(request.validate().unwrap_err().exit_code(), 2);
    }

    #[test]
    fn local_bucket_names_cannot_change_the_route() {
        for bucket in ["photos", "my.bucket", "a-b_c"] {
            assert!(validate_local_bucket(bucket).is_ok(), "{bucket}");
        }
        for bucket in ["", ".", "..", "a/b", "a b", "a%2Fb", "a?x", "a#f", "a\\b"] {
            assert_eq!(
                validate_local_bucket(bucket).unwrap_err().exit_code(),
                2,
                "{bucket}"
            );
        }
    }

    #[test]
    fn backfill_start_request_omits_unset_fields() {
        let request = BackfillStartRequest::default();
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({"dry_run": false})
        );
        let request = BackfillStartRequest {
            prefix: Some("photos/".into()),
            skip_existing: Some(SkipExisting::EtagOrSize),
            dry_run: true,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({"prefix": "photos/", "skip_existing": "etag_or_size", "dry_run": true})
        );
        assert!(
            BackfillStartRequest {
                prefix: Some(String::new()),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
}
