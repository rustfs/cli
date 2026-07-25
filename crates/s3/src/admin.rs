//! Admin API client implementation
//!
//! This module provides the AdminClient that implements the AdminApi trait
//! using HTTP requests with AWS SigV4 signing.

mod diagnostics;

use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SignatureLocation, SigningSettings, sign,
};
use aws_sigv4::sign::v4;
use bytes::Bytes;
use futures::StreamExt;
use rc_core::admin::{
    AccessKeyInfo, AccessKeyKind, AccessKeyProvider, AccessKeyRecord, AdminApi, BucketMetadataApi,
    BucketMetadataArchive, BucketQuota, BulkAccessKeyApi, BulkAccessKeyQuery, CapabilityApi,
    CapabilityAvailability, CapabilityEntry, CapabilityReport, ClusterInfo,
    ClusterSnapshotDocument, ClusterSnapshotMetadata, ClusterSnapshotSummary, ConfigApi,
    ConfigDocument, ConfigHelp, ConfigHistoryEntry, ConfigMutationResult,
    CreateServiceAccountRequest, DecommissionPoolStatus, DecommissionStatus,
    DetailedHealthSnapshot, DiagnosticCapability, DiagnosticReadApi, EncryptedInspectArchive,
    ExtensionsCatalog, Group, GroupStatus, HealRuntimeState, HealScanMode, HealStartRequest,
    HealStatus, HealTaskRequest, IAM_ACCESS_KEYS_BULK_CAPABILITY,
    IAM_ACCESS_KEYS_BULK_LDAP_CAPABILITY, IAM_ACCESS_KEYS_BULK_OPENID_CAPABILITY,
    IAM_POLICY_DETACH_CAPABILITY, IAM_POLICY_ENTITIES_CAPABILITY, INSPECT_ARCHIVE_COMPLETION,
    INSPECT_ARCHIVE_CONTENT_TYPE, IamArchiveApi, IamArchiveImportResult, IamArchiveImportSection,
    IamArchiveInventory, IamArchiveResultEntities, IamMutationApi, IamReadApi, InspectArchiveApi,
    InspectArchiveCapabilityContract, InspectArchiveTransportRequest, KmsApi, KmsBackendKind,
    KmsCacheSummary, KmsCancelKeyDeletionResult, KmsConfigSummary, KmsConfigureRequest,
    KmsCreateKeyRequest, KmsCreateKeyResult, KmsDeleteKeyRequest, KmsDeleteKeyResult, KmsKey,
    KmsKeyPage, KmsKeyState, KmsKeyUsage, KmsServiceState, KmsStatus,
    MAX_BUCKET_METADATA_ARCHIVE_BYTES, MAX_DIAGNOSTIC_RESPONSE_BYTES, MAX_IAM_ACCESS_KEY_RESULTS,
    MAX_IAM_ACCESS_KEYS_RESPONSE_BYTES, MAX_IAM_ARCHIVE_BYTES, MAX_IAM_IMPORT_RESPONSE_BYTES,
    MAX_IAM_POLICY_DETACH_REQUEST_BYTES, MAX_IAM_POLICY_DETACH_RESPONSE_BYTES,
    MAX_IAM_POLICY_ENTITIES_RESPONSE_BYTES, MAX_INSPECT_ARCHIVE_BYTES, MAX_METRICS_LINE_BYTES,
    MAX_METRICS_RESPONSE_BYTES, MAX_METRICS_SAMPLES, MAX_OIDC_RESPONSE_BYTES,
    MAX_REPLICATION_DIFF_RESPONSE_BYTES, MAX_REPLICATION_INSPECTION_RESPONSE_BYTES,
    MAX_SITE_REPLICATION_ERROR_RESPONSE_BYTES, MAX_SITE_REPLICATION_REQUEST_BYTES,
    MAX_SITE_REPLICATION_SUCCESS_RESPONSE_BYTES, ManualTransitionRunRequest,
    ManualTransitionRunResponse, MetricsBatch, MetricsQuery, ModuleSwitches, ObservabilityApi,
    OidcMutationApi, OidcMutationRequest, OidcMutationResult, OidcProvider, OidcProviderList,
    OidcReadApi, OidcValidationRequest, OidcValidationResult, PeerSiteSpec, Policy,
    PolicyDetachEntity, PolicyDetachRequest, PolicyDetachResult, PolicyEntitiesQuery,
    PolicyEntitiesResult, PolicyEntity, PolicyInfo, PoolStatus, PoolTarget, RealtimeMetrics,
    RebalanceStartResult, RebalanceStatus, ReplicateEditStatus, ReplicationDiff,
    ReplicationDiffApi, ReplicationInspectionApi, ReplicationMetricScope, ReplicationMetrics,
    ReplicationMrf, RuntimeCapabilitiesSnapshot, RuntimeCapabilityStatus, ScannerStatus,
    ServiceAccount, ServiceAccountCreateResponse, ServiceActionResult, SiteRemoveSpec,
    SiteReplicationInfo, SiteReplicationPeer, SiteReplicationResyncOperation,
    SiteReplicationResyncStatus, SiteStatusOptions, StorageInfo, UpdateGroupMembersRequest,
    UpdateServiceAccountRequest, User, UserStatus,
};
use rc_core::{Alias, Error, Result};
use reqwest::header::{CACHE_CONTROL, CONTENT_TYPE, HOST, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use zeroize::{Zeroize, Zeroizing};

struct SensitiveRequestBody(Zeroizing<Vec<u8>>);

struct BoundedJsonResponse {
    max_bytes: usize,
    name: &'static str,
    error_mapper: fn(&AdminClient, StatusCode, &str) -> Error,
}

#[derive(Serialize)]
struct InspectArchiveWireRequest<'a> {
    bucket: &'a str,
    object: &'a str,
    public_key_pem: &'a str,
}

#[derive(Debug, Serialize)]
struct PolicyDetachWireRequest<'a> {
    policies: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    group: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PolicyDetachWireResponse {
    #[serde(default)]
    policies_attached: Vec<String>,
    #[serde(default)]
    policies_detached: Vec<String>,
    updated_at: jiff::Timestamp,
}

#[derive(Debug, Default, Deserialize)]
struct WireIamImportFailures {
    #[serde(default)]
    policies: Vec<WireIamImportFailure>,
    #[serde(default)]
    users: Vec<WireIamImportFailure>,
    #[serde(default)]
    groups: Vec<WireIamImportFailure>,
    #[serde(default, rename = "serviceAccounts")]
    service_accounts: Vec<WireIamImportFailure>,
    #[serde(default, rename = "userPolicies")]
    user_policies: Vec<WireIamImportFailure>,
    #[serde(default, rename = "groupPolicies")]
    group_policies: Vec<WireIamImportFailure>,
    #[serde(default, rename = "stsPolicies")]
    sts_policies: Vec<WireIamImportFailure>,
}

#[derive(Debug, Default, Deserialize)]
struct WireIamImportFailure {
    #[serde(default)]
    name: String,
    #[serde(default)]
    policies: Vec<String>,
    // Deliberately omit the server's `error` field: it is untrusted and may
    // contain imported credential material.
}

#[derive(Debug, Default, Deserialize)]
struct WireIamImportResult {
    #[serde(default)]
    skipped: IamArchiveResultEntities,
    #[serde(default)]
    removed: IamArchiveResultEntities,
    #[serde(default)]
    added: IamArchiveResultEntities,
    #[serde(default)]
    failed: WireIamImportFailures,
}

impl WireIamImportResult {
    fn into_safe_result(self) -> IamArchiveImportResult {
        let mut failed = Vec::new();
        for entries in [
            self.failed.policies,
            self.failed.users,
            self.failed.groups,
            self.failed.service_accounts,
            self.failed.user_policies,
            self.failed.group_policies,
            self.failed.sts_policies,
        ] {
            failed.extend(entries.into_iter().map(|entry| IamArchiveImportSection {
                name: entry.name,
                policies: entry.policies,
            }));
        }
        failed.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.policies.cmp(&right.policies))
        });
        IamArchiveImportResult {
            skipped: self.skipped,
            removed: self.removed,
            added: self.added,
            failed,
        }
    }
}

impl AsRef<[u8]> for SensitiveRequestBody {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

#[cfg(test)]
use diagnostics::{
    CLIENT_DEVNULL_CHUNK_BYTES, client_devnull_payload_hash, next_client_devnull_chunk,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CapabilityCacheKey {
    endpoint: String,
    region: String,
    credential_fingerprint: String,
    transport_security_fingerprint: String,
}

static CAPABILITY_CACHE: OnceLock<Mutex<HashMap<CapabilityCacheKey, CapabilityReport>>> =
    OnceLock::new();

const MAX_CONFIG_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_BUCKET_METADATA_MUTATION_RESPONSE_BYTES: usize = 64 * 1024;

fn capability_cache() -> &'static Mutex<HashMap<CapabilityCacheKey, CapabilityReport>> {
    CAPABILITY_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn transport_security_fingerprint(
    insecure: bool,
    ca_bundle: Option<&[u8]>,
    client_cert: Option<&[u8]>,
    client_key: Option<&[u8]>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"rc-admin-capability-cache-transport-v1\0");
    hasher.update(b"native-roots-enabled\0webpki-roots-enabled\0");
    hasher.update([u8::from(insecure)]);

    for (label, input) in [
        (b"ca-bundle".as_slice(), ca_bundle),
        (b"client-certificate".as_slice(), client_cert),
        (b"client-private-key".as_slice(), client_key),
    ] {
        hasher.update(label);
        hasher.update([0]);
        match input {
            Some(bytes) => {
                hasher.update([1]);
                hasher.update(Sha256::digest(bytes));
            }
            None => hasher.update([0]),
        }
    }

    hex::encode(hasher.finalize())
}

/// Admin API client for RustFS servers
pub struct AdminClient {
    http_client: Client,
    endpoint: String,
    access_key: String,
    secret_key: String,
    region: String,
    anonymous: bool,
    transport_security_fingerprint: String,
}

impl AdminClient {
    /// Create a new AdminClient from an Alias
    pub fn new(alias: &Alias) -> Result<Self> {
        let mut builder = Client::builder()
            .danger_accept_invalid_certs(alias.insecure)
            .tls_built_in_native_certs(true)
            .tls_built_in_webpki_certs(true)
            .redirect(reqwest::redirect::Policy::none());

        if let Some(timeout) = &alias.timeout {
            if timeout.connect_ms == 0 || timeout.read_ms == 0 {
                return Err(Error::Config(
                    "Alias timeout values must be greater than zero".to_string(),
                ));
            }
            builder = builder
                .connect_timeout(Duration::from_millis(timeout.connect_ms))
                .read_timeout(Duration::from_millis(timeout.read_ms));
        }

        let ca_bundle_pem = if let Some(bundle_path) = alias.ca_bundle.as_deref() {
            let pem = std::fs::read(bundle_path).map_err(|e| {
                Error::Network(format!("Failed to read CA bundle '{bundle_path}': {e}"))
            })?;
            let certs = reqwest::Certificate::from_pem_bundle(&pem)
                .map_err(|e| Error::Network(format!("Invalid CA bundle '{bundle_path}': {e}")))?;
            if certs.is_empty() {
                return Err(Error::Network(format!(
                    "Invalid CA bundle '{bundle_path}': no certificates found"
                )));
            }
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
            Some(pem)
        } else {
            None
        };

        let client_identity_pem = if let (Some(cert_path), Some(key_path)) =
            (alias.client_cert.as_deref(), alias.client_key.as_deref())
        {
            let cert_pem = std::fs::read(cert_path).map_err(|e| {
                Error::Network(format!(
                    "Failed to read client certificate '{cert_path}': {e}"
                ))
            })?;
            let key_pem = std::fs::read(key_path).map_err(|e| {
                Error::Network(format!("Failed to read client key '{key_path}': {e}"))
            })?;
            let mut identity_pem = cert_pem.clone();
            identity_pem.extend_from_slice(b"\n");
            identity_pem.extend_from_slice(&key_pem);
            let identity = reqwest::Identity::from_pem(&identity_pem).map_err(|e| {
                Error::Network(format!("Invalid client certificate/key identity: {e}"))
            })?;
            builder = builder.use_rustls_tls().identity(identity);
            Some((cert_pem, key_pem))
        } else {
            None
        };

        let transport_security_fingerprint = transport_security_fingerprint(
            alias.insecure,
            ca_bundle_pem.as_deref(),
            client_identity_pem
                .as_ref()
                .map(|(certificate, _)| certificate.as_slice()),
            client_identity_pem
                .as_ref()
                .map(|(_, private_key)| private_key.as_slice()),
        );

        let http_client = builder
            .build()
            .map_err(|e| Error::Network(format!("Failed to create HTTP client: {e}")))?;

        Ok(Self {
            http_client,
            endpoint: alias.endpoint.trim_end_matches('/').to_string(),
            access_key: alias.access_key.clone(),
            secret_key: alias.secret_key.clone(),
            region: alias.region.clone(),
            anonymous: alias.anonymous,
            transport_security_fingerprint,
        })
    }

    /// Build the base URL for admin API
    fn admin_url(&self, path: &str) -> String {
        format!("{}/rustfs/admin/v3{}", self.endpoint, path)
    }

    fn admin_v4_url(&self, path: &str) -> String {
        format!("{}/rustfs/admin/v4{}", self.endpoint, path)
    }

    pub(crate) const fn http_client(&self) -> &Client {
        &self.http_client
    }

    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Calculate SHA256 hash of the body
    fn sha256_hash(body: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(body);
        hex::encode(hasher.finalize())
    }

    fn request_headers(&self, body: &[u8]) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        let content_hash = HeaderValue::from_str(&Self::sha256_hash(body))
            .map_err(|e| Error::Auth(format!("Invalid content hash header: {e}")))?;
        let host = HeaderValue::from_str(&self.get_host())
            .map_err(|e| Error::Auth(format!("Invalid host header: {e}")))?;

        headers.insert(
            HeaderName::from_static("x-amz-content-sha256"),
            content_hash,
        );
        headers.insert(HOST, host);
        if !body.is_empty() {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }

        Ok(headers)
    }

    /// Sign a request using AWS SigV4
    async fn sign_request(
        &self,
        method: &Method,
        url: &str,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<HeaderMap> {
        self.sign_request_with_body(method, url, headers, SignableBody::Bytes(body))
            .await
    }

    async fn sign_request_with_body(
        &self,
        method: &Method,
        url: &str,
        headers: &HeaderMap,
        signable_body: SignableBody<'_>,
    ) -> Result<HeaderMap> {
        if self.anonymous {
            return Ok(headers.clone());
        }

        let credentials = Credentials::new(
            &self.access_key,
            &self.secret_key,
            None,
            None,
            "admin-client",
        );

        let identity = credentials.into();
        let mut signing_settings = SigningSettings::default();
        signing_settings.signature_location = SignatureLocation::Headers;

        let signing_params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name("s3")
            .time(SystemTime::now())
            .settings(signing_settings)
            .build()
            .map_err(|e| Error::Auth(format!("Failed to build signing params: {e}")))?;

        // Convert headers to a vec of tuples
        let header_pairs: Vec<(&str, &str)> = headers
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str(), v)))
            .collect();

        let signable_request = SignableRequest::new(
            method.as_str(),
            url,
            header_pairs.into_iter(),
            signable_body,
        )
        .map_err(|e| Error::Auth(format!("Failed to create signable request: {e}")))?;

        let (signing_instructions, _signature) = sign(signable_request, &signing_params.into())
            .map_err(|e| Error::Auth(format!("Failed to sign request: {e}")))?
            .into_parts();

        // Apply signing instructions to create new headers
        let mut signed_headers = headers.clone();
        for (name, value) in signing_instructions.headers() {
            let header_name = HeaderName::try_from(&name.to_string())
                .map_err(|e| Error::Auth(format!("Invalid header name: {e}")))?;
            let header_value = HeaderValue::try_from(&value.to_string())
                .map_err(|e| Error::Auth(format!("Invalid header value: {e}")))?;
            signed_headers.insert(header_name, header_value);
        }

        Ok(signed_headers)
    }

    /// Make a signed request to the admin API
    pub(crate) async fn request<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        query: Option<&[(&str, &str)]>,
        body: Option<&[u8]>,
    ) -> Result<T> {
        self.request_url(method, self.admin_url(path), query, body)
            .await
    }

    async fn request_v4<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
    ) -> Result<T> {
        self.request_bounded_json_url(method, self.admin_v4_url(path))
            .await
    }

    async fn request_bounded_json_url<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        url: String,
    ) -> Result<T> {
        let headers = self.request_headers(&[])?;
        let signed_headers = self.sign_request(&method, &url, &headers, &[]).await?;
        let mut request_builder = self.http_client.request(method, &url);
        for (name, value) in signed_headers.iter() {
            request_builder = request_builder.header(name, value);
        }

        let response = request_builder
            .send()
            .await
            .map_err(|error| Error::Network(format!("Request failed: {error}")))?;
        let status = response.status();
        let body = read_bounded_diagnostic_body(response).await?;
        if !status.is_success() {
            return Err(self.map_error(status, &String::from_utf8_lossy(&body)));
        }
        serde_json::from_slice(&body).map_err(Error::Json)
    }

    async fn request_url<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        mut url: String,
        query: Option<&[(&str, &str)]>,
        body: Option<&[u8]>,
    ) -> Result<T> {
        if let Some(q) = query {
            let query_string: String = q
                .iter()
                .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
                .collect::<Vec<_>>()
                .join("&");
            if !query_string.is_empty() {
                url.push('?');
                url.push_str(&query_string);
            }
        }

        let body_bytes = body.unwrap_or(&[]);
        let headers = self.request_headers(body_bytes)?;

        let signed_headers = self
            .sign_request(&method, &url, &headers, body_bytes)
            .await?;

        let mut request_builder = self.http_client.request(method.clone(), &url);

        for (name, value) in signed_headers.iter() {
            request_builder = request_builder.header(name, value);
        }

        if !body_bytes.is_empty() {
            request_builder = request_builder.body(body_bytes.to_vec());
        }

        let response = request_builder
            .send()
            .await
            .map_err(|e| Error::Network(format!("Request failed: {e}")))?;

        let status = response.status();

        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(self.map_error(status, &error_body));
        }

        let text = response
            .text()
            .await
            .map_err(|e| Error::Network(format!("Failed to read response: {e}")))?;

        if text.is_empty() {
            // Return empty/default for empty responses
            serde_json::from_str("null").map_err(Error::Json)
        } else {
            serde_json::from_str(&text).map_err(Error::Json)
        }
    }

    async fn request_site_replication<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        body: Option<&[u8]>,
        operation_label: &'static str,
        mutation_outcome_label: Option<&'static str>,
        uncertain_response_label: Option<&'static str>,
    ) -> Result<T> {
        let body_bytes = body.unwrap_or(&[]);
        if body_bytes.len() > MAX_SITE_REPLICATION_REQUEST_BYTES {
            return Err(Error::RequestRejected(format!(
                "Site replication request size {} exceeds the {} byte limit",
                body_bytes.len(),
                MAX_SITE_REPLICATION_REQUEST_BYTES
            )));
        }

        let url = self.admin_url(path);
        let headers = self.request_headers(body_bytes)?;
        let signed_headers = self
            .sign_request(&method, &url, &headers, body_bytes)
            .await?;
        let mut request_builder = self.http_client.request(method, &url);
        for (name, value) in &signed_headers {
            request_builder = request_builder.header(name, value);
        }
        if !body_bytes.is_empty() {
            request_builder = request_builder.body(body_bytes.to_vec());
        }

        let response = request_builder.send().await.map_err(|error| {
            if let Some(mutation_outcome_label) = mutation_outcome_label {
                Error::Network(format!(
                    "{mutation_outcome_label} outcome is unknown; the request was not retried: {error}"
                ))
            } else {
                Error::Network(format!("Request failed: {error}"))
            }
        })?;
        let status = response.status();
        let limit = if status.is_success() {
            MAX_SITE_REPLICATION_SUCCESS_RESPONSE_BYTES
        } else {
            MAX_SITE_REPLICATION_ERROR_RESPONSE_BYTES
        };
        if response
            .content_length()
            .is_some_and(|content_length| content_length > limit as u64)
        {
            return Err(site_replication_response_rejected(
                uncertain_response_label,
                format!("response exceeds the {limit} byte limit"),
            ));
        }

        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                if let Some(mutation_outcome_label) = mutation_outcome_label {
                    Error::Network(format!(
                        "{mutation_outcome_label} outcome is unknown; the response could not be read and the request was not retried: {error}"
                    ))
                } else {
                    Error::Network(format!("Failed to read site replication response: {error}"))
                }
            })?;
            if bytes.len().saturating_add(chunk.len()) > limit {
                return Err(site_replication_response_rejected(
                    uncertain_response_label,
                    format!("response exceeds the {limit} byte limit"),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }

        if !status.is_success() {
            let error_body = String::from_utf8_lossy(&bytes);
            let error = self.map_site_replication_error(status, &error_body, operation_label);
            if !status.is_redirection()
                && matches!(error, Error::Network(_))
                && let Some(label) = uncertain_response_label
            {
                return Err(site_replication_response_unknown_network(
                    label,
                    status.as_u16(),
                ));
            }
            return Err(error);
        }
        serde_json::from_slice(&bytes).map_err(|error| {
            if uncertain_response_label.is_some() {
                site_replication_response_rejected(
                    uncertain_response_label,
                    "server returned malformed JSON".to_string(),
                )
            } else {
                Error::Json(error)
            }
        })
    }

    async fn request_bounded_json<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        query: Option<&[(&str, &str)]>,
        body: Option<&[u8]>,
        response: BoundedJsonResponse,
    ) -> Result<T> {
        let mut url = self.admin_url(path);
        if let Some(query) = query {
            let query_string = query
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{}={}",
                        urlencoding::encode(key),
                        urlencoding::encode(value)
                    )
                })
                .collect::<Vec<_>>()
                .join("&");
            if !query_string.is_empty() {
                url.push('?');
                url.push_str(&query_string);
            }
        }

        let body_bytes = body.unwrap_or(&[]);
        let headers = self.request_headers(body_bytes)?;
        let signed_headers = self
            .sign_request(&method, &url, &headers, body_bytes)
            .await?;
        let mut request_builder = self.http_client.request(method, &url);
        for (name, value) in signed_headers.iter() {
            request_builder = request_builder.header(name, value);
        }
        if !body_bytes.is_empty() {
            request_builder = request_builder.body(body_bytes.to_vec());
        }

        let http_response = request_builder
            .send()
            .await
            .map_err(|error| Error::Network(format!("Request failed: {error}")))?;
        let status = http_response.status();
        let response_body =
            read_bounded_response_body(http_response, response.max_bytes, response.name).await?;
        if !status.is_success() {
            return Err((response.error_mapper)(
                self,
                status,
                &String::from_utf8_lossy(&response_body),
            ));
        }

        serde_json::from_slice(&response_body).map_err(Error::Json)
    }

    async fn request_oidc<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<T> {
        let url = self.admin_url(path);
        let body_bytes = body.unwrap_or_default();
        let headers = self.request_headers(body_bytes)?;
        let signed_headers = self
            .sign_request(&method, &url, &headers, body_bytes)
            .await?;
        let mut request = self.http_client.request(method, &url);
        for (name, value) in &signed_headers {
            request = request.header(name, value);
        }
        if !body_bytes.is_empty() {
            request = request.body(body_bytes.to_vec());
        }

        let response = request
            .send()
            .await
            .map_err(|_| Error::Network("OIDC administration request failed".to_string()))?;
        let status = response.status();
        let response_body = read_bounded_response_body(
            response,
            MAX_OIDC_RESPONSE_BYTES,
            "OIDC administration response",
        )
        .await?;
        if !status.is_success() {
            return Err(match status {
                StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => {
                    Error::Auth("OIDC administration permission was denied".to_string())
                }
                StatusCode::NOT_FOUND
                | StatusCode::METHOD_NOT_ALLOWED
                | StatusCode::NOT_IMPLEMENTED => Error::UnsupportedFeature(
                    "The RustFS OIDC administration route is unavailable".to_string(),
                ),
                StatusCode::BAD_REQUEST => {
                    Error::General("OIDC issuer validation failed".to_string())
                }
                _ => Error::Network("OIDC administration service is unavailable".to_string()),
            });
        }

        serde_json::from_slice(&response_body)
            .map_err(|_| Error::General("RustFS returned a malformed OIDC response".to_string()))
    }

    async fn request_oidc_mutation<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: Zeroizing<Vec<u8>>,
    ) -> Result<T> {
        let url = self.admin_url(path);
        let headers = self.request_headers(body.as_slice())?;
        let signed_headers = self
            .sign_request(&Method::PUT, &url, &headers, body.as_slice())
            .await?;
        let mut request = self.http_client.put(&url);
        for (name, value) in &signed_headers {
            request = request.header(name, value);
        }
        let response = request
            .body(Bytes::from_owner(SensitiveRequestBody(body)))
            .send()
            .await
            .map_err(|_| Error::Network("OIDC administration request failed".to_string()))?;
        let status = response.status();
        let response_body = read_bounded_response_body(
            response,
            MAX_OIDC_RESPONSE_BYTES,
            "OIDC administration response",
        )
        .await?;
        if !status.is_success() {
            return Err(match status {
                StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => {
                    Error::Auth("OIDC administration permission was denied".to_string())
                }
                StatusCode::NOT_FOUND
                | StatusCode::METHOD_NOT_ALLOWED
                | StatusCode::NOT_IMPLEMENTED => Error::UnsupportedFeature(
                    "The RustFS OIDC mutation route is unavailable".to_string(),
                ),
                StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => {
                    Error::Conflict("OIDC provider changed concurrently".to_string())
                }
                StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
                    Error::General("OIDC configuration request was rejected".to_string())
                }
                _ => Error::Network("OIDC administration service is unavailable".to_string()),
            });
        }
        serde_json::from_slice(&response_body)
            .map_err(|_| Error::General("RustFS returned a malformed OIDC response".to_string()))
    }

    async fn request_oidc_delete<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let url = self.admin_url(path);
        let headers = self.request_headers(&[])?;
        let signed_headers = self
            .sign_request(&Method::DELETE, &url, &headers, &[])
            .await?;
        let mut request = self.http_client.delete(&url);
        for (name, value) in &signed_headers {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|_| Error::Network("OIDC administration request failed".to_string()))?;
        let status = response.status();
        let response_body = read_bounded_response_body(
            response,
            MAX_OIDC_RESPONSE_BYTES,
            "OIDC administration response",
        )
        .await?;
        if !status.is_success() {
            return Err(match status {
                StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => {
                    Error::Auth("OIDC administration permission was denied".to_string())
                }
                StatusCode::NOT_FOUND
                | StatusCode::METHOD_NOT_ALLOWED
                | StatusCode::NOT_IMPLEMENTED => Error::UnsupportedFeature(
                    "The RustFS OIDC deletion route is unavailable".to_string(),
                ),
                // RustFS deliberately reports an absent persisted provider as InvalidRequest.
                // Provider IDs are validated locally, so this status has deterministic retry
                // semantics instead of being collapsed into a generic server rejection.
                StatusCode::BAD_REQUEST => {
                    Error::NotFound("OIDC provider was not found".to_string())
                }
                StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => {
                    Error::Conflict("OIDC provider changed concurrently".to_string())
                }
                StatusCode::UNPROCESSABLE_ENTITY => {
                    Error::General("OIDC deletion request was rejected".to_string())
                }
                _ => Error::Network("OIDC administration service is unavailable".to_string()),
            });
        }
        serde_json::from_slice(&response_body)
            .map_err(|_| Error::General("RustFS returned a malformed OIDC response".to_string()))
    }

    /// Send a KMS request while retaining ownership of its body in zeroizing storage.
    async fn request_sensitive<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        body: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<T> {
        let url = self.admin_url(path);
        let body_bytes = body
            .as_ref()
            .map(|body| body.as_slice())
            .unwrap_or_default();
        let headers = self.request_headers(body_bytes)?;
        let signed_headers = self
            .sign_request(&method, &url, &headers, body_bytes)
            .await?;
        let mut request_builder = self.http_client.request(method, &url);
        for (name, value) in signed_headers.iter() {
            request_builder = request_builder.header(name, value);
        }
        if let Some(body) = body {
            request_builder = request_builder.body(Bytes::from_owner(SensitiveRequestBody(body)));
        }
        let response = request_builder
            .send()
            .await
            .map_err(|_| Error::Network("KMS administration request failed".to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(kms_lifecycle_status_error(status));
        }
        let text = Zeroizing::new(
            response
                .text()
                .await
                .map_err(|_| Error::Network("Failed to read KMS response".to_string()))?,
        );
        if text.is_empty() {
            serde_json::from_str("null").map_err(Error::Json)
        } else {
            serde_json::from_str(&text).map_err(Error::Json)
        }
    }

    /// Make a signed request that returns no body
    async fn request_no_response(
        &self,
        method: Method,
        path: &str,
        query: Option<&[(&str, &str)]>,
        body: Option<&[u8]>,
    ) -> Result<()> {
        let mut url = self.admin_url(path);

        if let Some(q) = query {
            let query_string: String = q
                .iter()
                .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
                .collect::<Vec<_>>()
                .join("&");
            if !query_string.is_empty() {
                url.push('?');
                url.push_str(&query_string);
            }
        }

        let body_bytes = body.unwrap_or(&[]);
        let headers = self.request_headers(body_bytes)?;

        let signed_headers = self
            .sign_request(&method, &url, &headers, body_bytes)
            .await?;

        let mut request_builder = self.http_client.request(method.clone(), &url);

        for (name, value) in signed_headers.iter() {
            request_builder = request_builder.header(name, value);
        }

        if !body_bytes.is_empty() {
            request_builder = request_builder.body(body_bytes.to_vec());
        }

        let response = request_builder
            .send()
            .await
            .map_err(|e| Error::Network(format!("Request failed: {e}")))?;

        let status = response.status();

        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(self.map_error(status, &error_body));
        }

        Ok(())
    }

    async fn config_request(
        &self,
        method: Method,
        path: &str,
        query: Option<&[(&str, &str)]>,
        body: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<(HeaderMap, Zeroizing<Vec<u8>>)> {
        let mut url = self.admin_url(path);
        if let Some(query) = query {
            let query_string = query
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{}={}",
                        urlencoding::encode(key),
                        urlencoding::encode(value)
                    )
                })
                .collect::<Vec<_>>()
                .join("&");
            if !query_string.is_empty() {
                url.push('?');
                url.push_str(&query_string);
            }
        }

        let body_bytes = body
            .as_ref()
            .map(|value| value.as_slice())
            .unwrap_or_default();
        let headers = self.request_headers(body_bytes)?;
        let signed_headers = self
            .sign_request(&method, &url, &headers, body_bytes)
            .await?;
        let mut request = self.http_client.request(method, &url);
        for (name, value) in &signed_headers {
            request = request.header(name, value);
        }
        if let Some(body) = body {
            request = request.body(Bytes::from_owner(SensitiveRequestBody(body)));
        }

        let response = request
            .send()
            .await
            .map_err(|_| Error::Network("Configuration request failed".to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        let response_body = Zeroizing::new(
            read_bounded_response_body(
                response,
                MAX_CONFIG_RESPONSE_BYTES,
                "configuration response",
            )
            .await?,
        );
        if !status.is_success() {
            return Err(safe_config_error(status));
        }
        Ok((headers, response_body))
    }

    /// Extract host from endpoint
    fn get_host(&self) -> String {
        self.endpoint
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .to_string()
    }

    /// Map HTTP status codes to appropriate errors
    pub(crate) fn map_error(&self, status: StatusCode, body: &str) -> Error {
        if matches!(status, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED) {
            return Error::Auth(body.to_string());
        }

        // Route absence is authoritative even when a proxy or compatibility layer returns a
        // stale structured error body. Capability discovery relies on 404 to identify servers
        // that predate the Admin API v4 route.
        if status == StatusCode::NOT_FOUND {
            return Error::NotFound(body.to_string());
        }

        let structured_error = parse_admin_error(body);
        if status == StatusCode::NOT_IMPLEMENTED
            || structured_error
                .as_ref()
                .is_some_and(|error| error.code.as_deref() == Some("NotImplemented"))
        {
            return Error::UnsupportedFeature(
                structured_error
                    .and_then(|error| error.message)
                    .unwrap_or_else(|| body.to_string()),
            );
        }

        if structured_error
            .as_ref()
            .is_some_and(AdminErrorResponse::is_missing_credentials)
        {
            return Error::Auth(body.to_string());
        }

        match status {
            StatusCode::CONFLICT => Error::Conflict(body.to_string()),
            StatusCode::BAD_REQUEST => Error::General(format!("Bad request: {body}")),
            _ => Error::Network(format!("HTTP {}: {}", status.as_u16(), body)),
        }
    }

    fn map_site_replication_error(
        &self,
        status: StatusCode,
        body: &str,
        operation_label: &str,
    ) -> Error {
        if matches!(
            status,
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
        ) {
            return Error::UnsupportedFeature(format!(
                "{operation_label} is not supported by this server"
            ));
        }
        if status == StatusCode::BAD_REQUEST
            && (site_replication_operation_conflicts(body)
                || (operation_label.starts_with("Site replication resync")
                    && site_replication_resync_conflicts(body)))
        {
            return Error::Conflict(
                "Site replication request conflicts with current server state".to_string(),
            );
        }
        if matches!(status, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED) {
            return Error::Auth("Authentication failed for site replication request".to_string());
        }
        if status == StatusCode::CONFLICT {
            return Error::Conflict("Site replication request conflicts with server state".into());
        }
        if status == StatusCode::BAD_REQUEST {
            return Error::General("Site replication request was rejected by the server".into());
        }
        Error::Network(format!(
            "Site replication request failed with HTTP {}",
            status.as_u16()
        ))
    }

    fn sanitize_site_replication_status(&self, status: &mut ReplicateEditStatus) {
        self.redact_admin_credentials(&mut status.status);
        self.redact_admin_credentials(&mut status.error_detail);
    }

    fn sanitize_site_replication_resync_status(&self, status: &mut SiteReplicationResyncStatus) {
        self.redact_admin_credentials(&mut status.operation);
        self.redact_admin_credentials(&mut status.resync_id);
        self.redact_admin_credentials(&mut status.status);
        self.redact_admin_credentials(&mut status.error_detail);
        for value in status.extensions.values_mut() {
            self.redact_admin_credentials_in_value(value);
        }
        for bucket in &mut status.buckets {
            self.redact_admin_credentials(&mut bucket.bucket);
            self.redact_admin_credentials(&mut bucket.status);
            self.redact_admin_credentials(&mut bucket.error_detail);
            for value in bucket.extensions.values_mut() {
                self.redact_admin_credentials_in_value(value);
            }
        }
    }

    fn redact_admin_credentials_in_value(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(value) => self.redact_admin_credentials(value),
            serde_json::Value::Array(values) => {
                for value in values {
                    self.redact_admin_credentials_in_value(value);
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values_mut() {
                    self.redact_admin_credentials_in_value(value);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }

    fn redact_admin_credentials(&self, value: &mut String) {
        let mut credentials = [&self.access_key, &self.secret_key];
        credentials.sort_by_key(|credential| std::cmp::Reverse(credential.len()));
        for credential in credentials {
            if !credential.is_empty() {
                *value = value.replace(credential, "[REDACTED]");
            }
        }
    }

    fn map_replication_diff_error(&self, status: StatusCode, body: &str) -> Error {
        if status != StatusCode::NOT_FOUND {
            return self.map_error(status, body);
        }

        let structured_error = parse_admin_error(body);
        if structured_error.as_ref().is_some_and(|error| {
            matches!(
                error.code.as_deref(),
                Some(
                    "NoSuchBucket"
                        | "ReplicationConfigurationNotFoundError"
                        | "ReplicationConfigurationNotFound"
                )
            )
        }) {
            return Error::NotFound(body.to_string());
        }

        let reason = structured_error
            .and_then(|error| error.message)
            .filter(|message| !message.trim().is_empty())
            .unwrap_or_else(|| "the replication diff route was not found".to_string());
        Error::UnsupportedFeature(reason)
    }

    fn map_replication_inspection_error(&self, status: StatusCode, body: &str) -> Error {
        if matches!(status, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED) {
            return Error::Auth("Authentication failed for replication inspection".into());
        }
        if matches!(
            status,
            StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
        ) {
            return Error::UnsupportedFeature(
                "Replication inspection is not supported by this server".into(),
            );
        }
        if status != StatusCode::NOT_FOUND {
            let mut redacted = body.to_string();
            self.redact_admin_credentials(&mut redacted);
            return self.map_error(status, &redacted);
        }
        let structured = parse_admin_error(body);
        let mut safe_message = structured
            .as_ref()
            .and_then(|error| error.message.clone())
            .filter(|message| !message.trim().is_empty());
        if let Some(message) = &mut safe_message {
            self.redact_admin_credentials(message);
        }
        if structured.as_ref().is_some_and(|error| {
            matches!(
                error.code.as_deref(),
                Some(
                    "NoSuchBucket"
                        | "ReplicationConfigurationNotFoundError"
                        | "ReplicationConfigurationNotFound"
                )
            )
        }) {
            return Error::NotFound(safe_message.unwrap_or_else(|| {
                "Bucket or replication configuration was not found".to_string()
            }));
        }
        Error::UnsupportedFeature(
            safe_message.unwrap_or_else(|| "Replication inspection route was not found".into()),
        )
    }

    fn sanitize_replication_scope(&self, scope: &mut Option<ReplicationMetricScope>) {
        if let Some(ReplicationMetricScope::Unknown(value)) = scope {
            self.redact_admin_credentials(value);
        }
    }

    fn sanitize_replication_metrics(&self, metrics: &mut ReplicationMetrics) {
        self.sanitize_replication_scope(&mut metrics.queue_scope);
        for value in metrics.extra.values_mut() {
            self.redact_admin_credentials_in_value(value);
        }
        for (arn, mut target) in std::mem::take(&mut metrics.stats) {
            let mut redacted_arn = arn;
            self.redact_admin_credentials(&mut redacted_arn);
            self.sanitize_replication_scope(&mut target.latency_scope);
            self.sanitize_replication_scope(&mut target.bandwidth_scope);
            for value in target.extra.values_mut() {
                self.redact_admin_credentials_in_value(value);
            }
            metrics.stats.insert(redacted_arn, target);
        }
    }

    fn sanitize_replication_mrf(&self, mrf: &mut ReplicationMrf) {
        self.redact_admin_credentials(&mut mrf.bucket);
        for target in &mut mrf.targets {
            self.redact_admin_credentials(&mut target.arn);
            if let ReplicationMetricScope::Unknown(value) = &mut target.observation_scope {
                self.redact_admin_credentials(value);
            }
            for value in target.extra.values_mut() {
                self.redact_admin_credentials_in_value(value);
            }
        }
        for value in mrf.extra.values_mut() {
            self.redact_admin_credentials_in_value(value);
        }
    }

    fn map_inspect_archive_error(&self, status: StatusCode, body: &str) -> Error {
        if matches!(status, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED) {
            return Error::Auth(
                "Permission denied while requesting the diagnostic archive".to_string(),
            );
        }
        let code = parse_admin_error(body).and_then(|error| error.code);
        if status == StatusCode::NOT_FOUND
            && matches!(
                code.as_deref(),
                Some("NoSuchBucket" | "NoSuchKey" | "NoSuchObject")
            )
        {
            return Error::NotFound("Diagnostic archive target was not found".to_string());
        }
        match status {
            StatusCode::NOT_FOUND
            | StatusCode::METHOD_NOT_ALLOWED
            | StatusCode::NOT_IMPLEMENTED => Error::UnsupportedFeature(
                "Encrypted diagnostic archives are not supported by this RustFS server".to_string(),
            ),
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => Error::InvalidPath(
                "RustFS rejected the diagnostic archive target or public key".to_string(),
            ),
            StatusCode::PAYLOAD_TOO_LARGE => Error::RequestRejected(
                "RustFS rejected the diagnostic archive because a server limit was exceeded"
                    .to_string(),
            ),
            StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => {
                Error::Network("Diagnostic archive request timed out".to_string())
            }
            StatusCode::TOO_MANY_REQUESTS => Error::RequestRejected(
                "RustFS diagnostic archive capacity limit was reached".to_string(),
            ),
            _ => Error::Network(format!(
                "Diagnostic archive request failed with HTTP {}",
                status.as_u16()
            )),
        }
    }

    fn map_iam_policy_entities_error(&self, status: StatusCode, _body: &str) -> Error {
        match status {
            StatusCode::NOT_FOUND
            | StatusCode::METHOD_NOT_ALLOWED
            | StatusCode::NOT_IMPLEMENTED => Error::UnsupportedFeature(
                "IAM policy-entity inspection is not supported by this RustFS server".to_string(),
            ),
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => {
                Error::Auth("Permission denied for IAM policy-entity inspection".to_string())
            }
            StatusCode::BAD_REQUEST => {
                Error::InvalidPath("RustFS rejected the IAM policy-entity query".to_string())
            }
            _ => Error::Network(format!(
                "IAM policy-entity inspection failed with HTTP {}",
                status.as_u16()
            )),
        }
    }

    fn map_iam_policy_detach_error(&self, status: StatusCode, body: &str) -> Error {
        let structured = parse_admin_error(body);
        let is_missing_entity = structured.as_ref().is_some_and(|error| {
            let code = error
                .code
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let message = error
                .message
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            code.contains("nosuchuser")
                || code.contains("nosuchgroup")
                || message.contains("user not exist")
                || message.contains("group not exist")
                || message.contains("no such user")
                || message.contains("no such group")
        });
        if is_missing_entity {
            return Error::NotFound("The requested IAM entity does not exist".to_string());
        }

        match status {
            StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED => {
                Error::UnsupportedFeature(
                    "Builtin IAM policy detach is not supported by this RustFS server".to_string(),
                )
            }
            StatusCode::NOT_FOUND if structured.is_none() => Error::UnsupportedFeature(
                "Builtin IAM policy detach is not supported by this RustFS server".to_string(),
            ),
            StatusCode::NOT_FOUND => {
                Error::NotFound("The requested IAM entity was not found".to_string())
            }
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => {
                Error::Auth("Permission denied for IAM policy detach".to_string())
            }
            StatusCode::BAD_REQUEST => {
                Error::InvalidPath("RustFS rejected the policy detach selectors".to_string())
            }
            StatusCode::CONFLICT => {
                Error::Conflict("IAM policy mappings changed concurrently".to_string())
            }
            _ => Error::Network(format!(
                "IAM policy detach failed with HTTP {}",
                status.as_u16()
            )),
        }
    }

    fn map_iam_access_keys_error(&self, status: StatusCode, _body: &str) -> Error {
        match status {
            StatusCode::NOT_FOUND
            | StatusCode::METHOD_NOT_ALLOWED
            | StatusCode::NOT_IMPLEMENTED => Error::UnsupportedFeature(
                "Bulk access-key inspection is not supported by this RustFS server".to_string(),
            ),
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => {
                Error::Auth("Permission denied for bulk access-key inspection".to_string())
            }
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
                Error::InvalidPath("RustFS rejected the bulk access-key query".to_string())
            }
            _ => Error::Network(format!(
                "Bulk access-key inspection failed with HTTP {}",
                status.as_u16()
            )),
        }
    }
}

fn site_replication_response_rejected(
    mutation_outcome_label: Option<&str>,
    reason: String,
) -> Error {
    if let Some(label) = mutation_outcome_label {
        Error::General(format!(
            "{label} outcome is unknown because the {reason}; do not retry blindly; inspect the persisted resync snapshot and storage state"
        ))
    } else {
        Error::General(format!("Site replication {reason}"))
    }
}

fn site_replication_response_unknown_network(label: &str, status: u16) -> Error {
    Error::Network(format!(
        "{label} outcome is unknown after the server returned HTTP {status}; do not retry blindly; inspect the persisted resync snapshot and storage state"
    ))
}

#[derive(Debug, Deserialize)]
struct AdminErrorResponse {
    #[serde(default, alias = "Code")]
    code: Option<String>,
    #[serde(default, alias = "Message")]
    message: Option<String>,
}

impl AdminErrorResponse {
    fn is_missing_credentials(&self) -> bool {
        self.code.as_deref() == Some("InvalidRequest")
            && matches!(
                self.message.as_deref().map(str::trim),
                Some("get cred failed" | "authentication required" | "missing credentials")
            )
    }
}

fn site_replication_operation_conflicts(body: &str) -> bool {
    let Some(error) = parse_admin_error(body) else {
        return false;
    };
    site_replication_server_invalid_state(
        error.code.as_deref(),
        error.message.as_deref().unwrap_or_default(),
    )
}

fn site_replication_resync_conflicts(body: &str) -> bool {
    let Some(error) = parse_admin_error(body) else {
        return false;
    };
    if error.code.as_deref() != Some("InvalidRequest") {
        return false;
    }

    matches!(
        error
            .message
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "no resync in progress"
            | "invalid peer specified - cannot resync to self"
            | "site replication peer not found"
    )
}

fn validate_site_replication_resync_status(
    requested_operation: &SiteReplicationResyncOperation,
    status: &SiteReplicationResyncStatus,
) -> Result<()> {
    if status.operation.trim().is_empty() {
        return Err(Error::General(
            "Site replication resync response is missing the operation".to_string(),
        ));
    }
    if status.status.trim().is_empty() {
        return Err(Error::General(
            "Site replication resync response is missing the status".to_string(),
        ));
    }
    if requested_operation.is_mutation()
        && !status
            .operation
            .trim()
            .eq_ignore_ascii_case(requested_operation.as_str())
    {
        return Err(Error::General(
            "Site replication resync response operation does not match the requested mutation"
                .to_string(),
        ));
    }
    let response_is_mutation = matches!(
        status.operation.trim().to_ascii_lowercase().as_str(),
        "start" | "cancel"
    );
    let is_no_snapshot = *requested_operation == SiteReplicationResyncOperation::Status
        && status.operation.trim().eq_ignore_ascii_case("status")
        && status.status.trim().eq_ignore_ascii_case("not-found");
    if (requested_operation.is_mutation() || response_is_mutation)
        && !is_no_snapshot
        && status.resync_id.trim().is_empty()
    {
        return Err(Error::General(
            "Site replication resync response is missing the operation ID".to_string(),
        ));
    }
    if status
        .buckets
        .iter()
        .any(|bucket| bucket.bucket.trim().is_empty() || bucket.status.trim().is_empty())
    {
        return Err(Error::General(
            "Site replication resync response contains an incomplete bucket record".to_string(),
        ));
    }
    Ok(())
}

fn site_replication_server_invalid_state(code: Option<&str>, message: &str) -> bool {
    if code.is_some_and(|code| {
        matches!(
            code,
            "SiteReplicationIAMChangePending"
                | "SiteReplicationOperationPending"
                | "SiteReplicationPeerEditPending"
        )
    }) {
        return true;
    }

    let message = message.trim().to_ascii_lowercase();
    if matches!(
        message.as_str(),
        "site replication operation pending"
            | "site replication peer edit pending"
            | "site replication iam change pending"
    ) {
        return true;
    }

    let state_changed = matches!(
        message.as_str(),
        "site replication state changed" | "site replication refresh state changed"
    );
    state_changed && matches!(code, None | Some("InvalidRequest"))
}

fn parse_admin_error(body: &str) -> Option<AdminErrorResponse> {
    serde_json::from_str(body)
        .ok()
        .or_else(|| quick_xml::de::from_str(body).ok())
}

async fn read_bounded_diagnostic_body(response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DIAGNOSTIC_RESPONSE_BYTES as u64)
    {
        return Err(Error::General(
            "Diagnostic response exceeded the 8 MiB limit".to_string(),
        ));
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| Error::Network(format!("Failed to read response: {error}")))?;
        if body.len().saturating_add(chunk.len()) > MAX_DIAGNOSTIC_RESPONSE_BYTES {
            return Err(Error::General(
                "Diagnostic response exceeded the 8 MiB limit".to_string(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn read_bounded_response_body(
    response: reqwest::Response,
    max_response_bytes: usize,
    response_name: &str,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > max_response_bytes as u64)
    {
        return Err(Error::General(format!(
            "{response_name} exceeded the {max_response_bytes}-byte response limit"
        )));
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| Error::Network(format!("Failed to read response: {error}")))?;
        if body.len().saturating_add(chunk.len()) > max_response_bytes {
            return Err(Error::General(format!(
                "{response_name} exceeded the {max_response_bytes}-byte response limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

/// Response wrapper for user list
#[derive(Debug, Deserialize)]
struct UserListResponse(HashMap<String, UserInfo>);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserInfo {
    #[serde(default)]
    status: String,
    #[serde(default)]
    policy_name: Option<String>,
    #[serde(default)]
    member_of: Option<Vec<String>>,
}

/// Response wrapper for policy list
#[derive(Debug, Deserialize)]
struct PolicyListResponse(HashMap<String, serde_json::Value>);

/// Request body for creating a user
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateUserRequest {
    secret_key: String,
    status: String,
}

/// Request body for creating a group
#[derive(Debug, Serialize)]
struct CreateGroupRequest {
    group: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    members: Option<Vec<String>>,
}

/// Response for service account list
#[derive(Debug, Deserialize)]
struct ServiceAccountListResponse {
    accounts: Option<Vec<ServiceAccountInfo>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServiceAccountInfo {
    access_key: String,
    #[serde(default)]
    parent_user: Option<String>,
    #[serde(default)]
    account_status: Option<String>,
    #[serde(default)]
    expiration: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    implied_policy: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackgroundHealStatusResponse {
    #[serde(default)]
    state: Option<HealRuntimeState>,
    #[serde(default)]
    bitrot_start_time: Option<String>,
    #[serde(default)]
    bitrot_start_cycle: u64,
    #[serde(default)]
    current_scan_mode: Option<u8>,
    #[serde(default)]
    heal_queue_length: u64,
    #[serde(default)]
    heal_active_tasks: u64,
    #[serde(default)]
    heal_operations: Option<BackgroundHealOperations>,
    #[serde(default)]
    progress: Option<HealProgressResponse>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackgroundHealOperations {
    #[serde(default)]
    queue_length: u64,
    #[serde(default)]
    active_tasks: u64,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct HealProgressResponse {
    #[serde(default)]
    objects_scanned: u64,
    #[serde(default)]
    objects_healed: u64,
    #[serde(default)]
    objects_failed: u64,
    #[serde(default)]
    bytes_scanned: u64,
    #[serde(default)]
    bytes_healed: u64,
    #[serde(default)]
    bytes_processed: u64,
}

impl HealProgressResponse {
    fn apply_to_status(&self, status: &mut HealStatus) {
        status.items_scanned = self.objects_scanned;
        status.items_healed = self.objects_healed;
        status.items_failed = self.objects_failed;
        status.bytes_scanned = self.bytes_scanned.max(self.bytes_processed);
        status.bytes_healed = self.bytes_healed;
    }
}

impl From<BackgroundHealStatusResponse> for HealStatus {
    fn from(response: BackgroundHealStatusResponse) -> Self {
        let scan_mode = background_heal_scan_mode(response.current_scan_mode);
        let legacy_healing = scan_mode.is_none() && response.bitrot_start_time.is_some();
        let queue_length = response
            .heal_operations
            .as_ref()
            .map_or(response.heal_queue_length, |operations| {
                response.heal_queue_length.max(operations.queue_length)
            });
        let active_tasks = response
            .heal_operations
            .as_ref()
            .map_or(response.heal_active_tasks, |operations| {
                response.heal_active_tasks.max(operations.active_tasks)
            });

        let legacy_status_healing = matches!(scan_mode, Some(HealScanMode::Deep))
            || queue_length > 0
            || active_tasks > 0
            || legacy_healing;
        let healing = match response.state {
            Some(HealRuntimeState::Active) => true,
            Some(
                HealRuntimeState::Disabled
                | HealRuntimeState::Uninitialized
                | HealRuntimeState::Idle,
            ) => false,
            Some(HealRuntimeState::Unknown) | None => legacy_status_healing,
        };

        let mut status = Self {
            healing,
            started: response.bitrot_start_time,
            scan_mode,
            scan_cycle: response.bitrot_start_cycle,
            heal_queue_length: queue_length,
            heal_active_tasks: active_tasks,
            ..Default::default()
        };
        status.state = response.state;
        if let Some(progress) = response.progress {
            progress.apply_to_status(&mut status);
        }
        status
    }
}

fn background_heal_scan_mode(scan_mode: Option<u8>) -> Option<HealScanMode> {
    match scan_mode {
        Some(1) => Some(HealScanMode::Normal),
        Some(2) => Some(HealScanMode::Deep),
        _ => None,
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct RustfsHealOptions {
    recursive: bool,
    #[serde(rename = "dryRun")]
    dry_run: bool,
    remove: bool,
    recreate: bool,
    #[serde(rename = "scanMode")]
    scan_mode: u8,
    #[serde(rename = "updateParity")]
    update_parity: bool,
    #[serde(rename = "nolock")]
    no_lock: bool,
}

impl From<&HealStartRequest> for RustfsHealOptions {
    fn from(request: &HealStartRequest) -> Self {
        Self::from_request(request, false)
    }
}

impl RustfsHealOptions {
    fn from_request(request: &HealStartRequest, recursive: bool) -> Self {
        Self {
            recursive,
            dry_run: request.dry_run,
            remove: request.remove,
            recreate: request.recreate,
            scan_mode: rustfs_heal_scan_mode(request.scan_mode),
            update_parity: false,
            no_lock: false,
        }
    }
}

fn rustfs_heal_scan_mode(scan_mode: HealScanMode) -> u8 {
    match scan_mode {
        HealScanMode::Normal => 1,
        HealScanMode::Deep => 2,
    }
}

fn rustfs_heal_path(request: &HealStartRequest) -> Result<String> {
    let bucket = request
        .bucket
        .as_deref()
        .filter(|bucket| !bucket.is_empty());
    let prefix = request
        .prefix
        .as_deref()
        .filter(|prefix| !prefix.is_empty());

    match (bucket, prefix) {
        (None, None) => Ok("/heal/".to_string()),
        (Some(bucket), None) => Ok(format!("/heal/{}", urlencoding::encode(bucket))),
        (Some(bucket), Some(prefix)) => Ok(format!(
            "/heal/{}/{}",
            urlencoding::encode(bucket),
            urlencoding::encode(prefix)
        )),
        (None, Some(_)) => Err(Error::InvalidPath(
            "heal prefix requires a bucket target".to_string(),
        )),
    }
}

fn rustfs_heal_task_path(request: &HealTaskRequest) -> Result<String> {
    let bucket = (!request.bucket.is_empty()).then_some(request.bucket.as_str());
    let prefix = request
        .prefix
        .as_deref()
        .filter(|prefix| !prefix.is_empty());

    match (bucket, prefix) {
        (None, None) => Ok("/heal/".to_string()),
        (Some(bucket), None) => Ok(format!("/heal/{}", urlencoding::encode(bucket))),
        (Some(bucket), Some(prefix)) => Ok(format!(
            "/heal/{}/{}",
            urlencoding::encode(bucket),
            urlencoding::encode(prefix)
        )),
        (None, Some(_)) => Err(Error::InvalidPath(
            "heal task prefix requires a bucket target".to_string(),
        )),
    }
}

fn rustfs_heal_body(request: &HealStartRequest) -> Result<Vec<u8>> {
    serde_json::to_vec(&RustfsHealOptions::from(request)).map_err(Error::Json)
}

fn rustfs_heal_start_body(request: &HealStartRequest) -> Result<Vec<u8>> {
    let recursive = request
        .bucket
        .as_deref()
        .is_none_or(|bucket| bucket.is_empty())
        && request
            .prefix
            .as_deref()
            .is_none_or(|prefix| prefix.is_empty());
    serde_json::to_vec(&RustfsHealOptions::from_request(request, recursive)).map_err(Error::Json)
}

fn pool_target_query(target: &PoolTarget) -> Vec<(&str, &str)> {
    let mut query = vec![("pool", target.pool.as_str())];
    if target.by_id {
        query.push(("by-id", "true"));
    }
    query
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HealStartSuccessResponse {
    #[serde(default)]
    client_token: String,
    #[serde(default, rename = "clientAddress")]
    _client_address: String,
    #[serde(default)]
    start_time: Option<String>,
}

impl HealStartSuccessResponse {
    fn into_status(self, request: &HealStartRequest) -> HealStatus {
        HealStatus {
            heal_id: self.client_token,
            bucket: request.bucket.clone().unwrap_or_default(),
            object: request.prefix.clone().unwrap_or_default(),
            started: self.start_time,
            ..Default::default()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HealTaskStatusResponse {
    #[serde(default)]
    summary: String,
    #[serde(default, rename = "detail")]
    detail: String,
    #[serde(default)]
    start_time: Option<String>,
    #[serde(default)]
    settings: Option<RustfsHealOptions>,
    #[serde(default)]
    progress: Option<HealProgressResponse>,
}

impl HealTaskStatusResponse {
    fn into_status(self, request: &HealTaskRequest) -> HealStatus {
        let healing = matches!(self.summary.as_str(), "running");
        let scan_mode = self.settings.map(|settings| {
            background_heal_scan_mode(Some(settings.scan_mode)).unwrap_or(HealScanMode::Normal)
        });

        let mut status = HealStatus {
            heal_id: request.client_token.clone(),
            healing,
            summary: (!self.summary.is_empty()).then_some(self.summary),
            detail: (!self.detail.is_empty()).then_some(self.detail),
            bucket: request.bucket.clone(),
            object: request.prefix.clone().unwrap_or_default(),
            scan_mode,
            started: self.start_time,
            ..Default::default()
        };
        if let Some(progress) = self.progress {
            progress.apply_to_status(&mut status);
        }
        status
    }
}

/// Request body for setting bucket quota
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetBucketQuotaApiRequest {
    quota: u64,
    quota_type: String,
}

#[derive(Debug, Deserialize)]
struct ServerInfoResponse {
    info: ClusterInfo,
}

#[derive(Debug, Deserialize)]
struct PoolStatusResponse {
    pool: PoolStatus,
}

#[derive(Debug, Deserialize)]
struct ClusterSnapshotResponse {
    snapshot: Option<ClusterSnapshotPayload>,
}

#[derive(Debug, Deserialize)]
struct StorageInfoResponse {
    info: StorageInfo,
}

#[derive(Debug, Deserialize)]
struct ClusterSnapshotPayload {
    summary: ClusterSnapshotSummary,
    runtime_capabilities_path: String,
    extensions_catalog_path: String,
}

impl AdminClient {
    fn capability_cache_key(&self) -> CapabilityCacheKey {
        let mut hasher = Sha256::new();
        if self.anonymous {
            hasher.update(b"anonymous");
        } else {
            hasher.update(b"authenticated\0");
            hasher.update(self.access_key.as_bytes());
            hasher.update(b"\0");
            hasher.update(self.secret_key.as_bytes());
        }

        CapabilityCacheKey {
            endpoint: self.endpoint.clone(),
            region: self.region.clone(),
            credential_fingerprint: hex::encode(hasher.finalize()),
            transport_security_fingerprint: self.transport_security_fingerprint.clone(),
        }
    }

    fn cached_capabilities(&self) -> Result<Option<CapabilityReport>> {
        capability_cache()
            .lock()
            .map(|cache| cache.get(&self.capability_cache_key()).cloned())
            .map_err(|_| Error::General("Capability cache lock is poisoned".to_string()))
    }

    fn store_capabilities(&self, report: &CapabilityReport) -> Result<()> {
        capability_cache()
            .lock()
            .map_err(|_| Error::General("Capability cache lock is poisoned".to_string()))?
            .insert(self.capability_cache_key(), report.clone());
        Ok(())
    }

    async fn discover_capabilities_uncached(&self) -> Result<CapabilityReport> {
        let cluster_info = self
            .request_bounded_json_url::<ServerInfoResponse>(Method::GET, self.admin_url("/info"))
            .await?
            .info;
        let server_version = cluster_info.servers.as_ref().and_then(|servers| {
            servers
                .iter()
                .map(|server| server.version.trim())
                .find(|version| !version.is_empty())
                .map(str::to_string)
        });

        let runtime = match self
            .request_v4::<RuntimeCapabilitiesSnapshot>(Method::GET, "/runtime/capabilities")
            .await
        {
            Ok(runtime) => runtime,
            Err(Error::NotFound(_)) => {
                return Ok(version_gated_report(server_version));
            }
            Err(Error::UnsupportedFeature(reason)) => {
                return Ok(stubbed_report(server_version, reason));
            }
            Err(error) => return Err(error),
        };

        let mut capabilities = runtime_summary_entries(&runtime);
        let extensions = match self
            .request_v4::<ExtensionsCatalog>(Method::GET, "/extensions/catalog")
            .await
        {
            Ok(catalog) => {
                capabilities.push(CapabilityEntry {
                    name: "admin.extensions-catalog".to_string(),
                    availability: CapabilityAvailability::Available,
                    reason: None,
                });
                catalog.extensions
            }
            Err(Error::NotFound(_)) => {
                capabilities.push(version_gated_entry(
                    "admin.extensions-catalog",
                    "The extensions catalog route is not available on this server version",
                ));
                Vec::new()
            }
            Err(Error::UnsupportedFeature(reason)) => {
                capabilities.push(CapabilityEntry {
                    name: "admin.extensions-catalog".to_string(),
                    availability: CapabilityAvailability::Stubbed,
                    reason: Some(reason),
                });
                Vec::new()
            }
            Err(error) => return Err(error),
        };

        let cluster = match self
            .request_v4::<ClusterSnapshotResponse>(Method::GET, "/cluster/snapshot")
            .await
        {
            Ok(response) => {
                capabilities.push(CapabilityEntry {
                    name: "admin.cluster-snapshot-route".to_string(),
                    availability: CapabilityAvailability::Available,
                    reason: response
                        .snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.summary.runtime.reason.clone()),
                });
                response.snapshot.map_or(
                    ClusterSnapshotMetadata {
                        summary: None,
                        runtime_capabilities_path: None,
                        extensions_catalog_path: None,
                    },
                    |snapshot| ClusterSnapshotMetadata {
                        summary: Some(snapshot.summary),
                        runtime_capabilities_path: Some(snapshot.runtime_capabilities_path),
                        extensions_catalog_path: Some(snapshot.extensions_catalog_path),
                    },
                )
            }
            Err(Error::NotFound(_)) => {
                capabilities.push(version_gated_entry(
                    "admin.cluster-snapshot-route",
                    "The cluster snapshot route is not available on this server version",
                ));
                ClusterSnapshotMetadata {
                    summary: None,
                    runtime_capabilities_path: None,
                    extensions_catalog_path: None,
                }
            }
            Err(Error::UnsupportedFeature(reason)) => {
                capabilities.push(CapabilityEntry {
                    name: "admin.cluster-snapshot-route".to_string(),
                    availability: CapabilityAvailability::Stubbed,
                    reason: Some(reason),
                });
                ClusterSnapshotMetadata {
                    summary: None,
                    runtime_capabilities_path: None,
                    extensions_catalog_path: None,
                }
            }
            Err(error) => return Err(error),
        };

        add_known_server_capabilities(server_version.as_deref(), &mut capabilities);
        capabilities.sort_by(|left, right| left.name.cmp(&right.name));

        Ok(CapabilityReport {
            server_version,
            runtime_path: "/rustfs/admin/v4/runtime/capabilities".to_string(),
            extensions_path: "/rustfs/admin/v4/extensions/catalog".to_string(),
            cluster_snapshot_path: runtime.cluster_snapshot_path,
            capabilities,
            extensions,
            cluster,
        })
    }
}

fn runtime_summary_entries(runtime: &RuntimeCapabilitiesSnapshot) -> Vec<CapabilityEntry> {
    let mut entries = [
        ("runtime.observability", &runtime.summary.observability),
        (
            "runtime.userspace-profiling",
            &runtime.summary.userspace_profiling,
        ),
        ("runtime.memory-sampling", &runtime.summary.memory_sampling),
        ("runtime.platform", &runtime.summary.platform),
        ("runtime.topology", &runtime.summary.topology),
        (
            "runtime.cluster-snapshot",
            &runtime.summary.cluster_snapshot,
        ),
    ]
    .into_iter()
    .map(|(name, status)| capability_entry(name, status))
    .collect::<Vec<_>>();
    if let Some(contract) = &runtime.inspect_archive {
        entries.push(CapabilityEntry {
            name: DiagnosticCapability::InspectArchive.name().to_string(),
            availability: contract.state.availability(),
            reason: contract.state.reason.clone(),
        });
    }
    entries
}

fn capability_entry(name: &str, status: &RuntimeCapabilityStatus) -> CapabilityEntry {
    CapabilityEntry {
        name: name.to_string(),
        availability: status.availability(),
        reason: status.reason.clone(),
    }
}

fn version_gated_entry(name: &str, reason: &str) -> CapabilityEntry {
    CapabilityEntry {
        name: name.to_string(),
        availability: CapabilityAvailability::VersionGated,
        reason: Some(reason.to_string()),
    }
}

fn version_gated_report(server_version: Option<String>) -> CapabilityReport {
    let reason = "RustFS Admin API v4 capability discovery is not available on this server version";
    let mut capabilities = vec![version_gated_entry("admin.runtime-capabilities", reason)];
    add_uniform_diagnostic_capabilities(
        &mut capabilities,
        CapabilityAvailability::VersionGated,
        reason,
    );
    CapabilityReport {
        server_version,
        runtime_path: "/rustfs/admin/v4/runtime/capabilities".to_string(),
        extensions_path: "/rustfs/admin/v4/extensions/catalog".to_string(),
        cluster_snapshot_path: "/rustfs/admin/v4/cluster/snapshot".to_string(),
        capabilities,
        extensions: Vec::new(),
        cluster: ClusterSnapshotMetadata {
            summary: None,
            runtime_capabilities_path: None,
            extensions_catalog_path: None,
        },
    }
}

fn stubbed_report(server_version: Option<String>, reason: String) -> CapabilityReport {
    let diagnostic_reason = format!(
        "Diagnostic support cannot be classified because runtime capability discovery is stubbed: {reason}"
    );
    let mut capabilities = vec![CapabilityEntry {
        name: "admin.runtime-capabilities".to_string(),
        availability: CapabilityAvailability::Stubbed,
        reason: Some(reason),
    }];
    add_uniform_diagnostic_capabilities(
        &mut capabilities,
        CapabilityAvailability::Unknown,
        &diagnostic_reason,
    );
    CapabilityReport {
        server_version,
        runtime_path: "/rustfs/admin/v4/runtime/capabilities".to_string(),
        extensions_path: "/rustfs/admin/v4/extensions/catalog".to_string(),
        cluster_snapshot_path: "/rustfs/admin/v4/cluster/snapshot".to_string(),
        capabilities,
        extensions: Vec::new(),
        cluster: ClusterSnapshotMetadata {
            summary: None,
            runtime_capabilities_path: None,
            extensions_catalog_path: None,
        },
    }
}

fn add_known_server_capabilities(version: Option<&str>, capabilities: &mut Vec<CapabilityEntry>) {
    if !version.is_some_and(is_rustfs_beta_10) {
        add_uniform_diagnostic_capabilities(
            capabilities,
            CapabilityAvailability::Unknown,
            "No pinned diagnostic capability contract is available for this server version",
        );
        mirror_read_only_diagnostic_routes(capabilities);
        return;
    }

    capabilities.push(CapabilityEntry {
        name: "admin.data-usage".to_string(),
        availability: CapabilityAvailability::Available,
        reason: None,
    });
    capabilities.push(CapabilityEntry {
        name: "listen_notification".to_string(),
        availability: CapabilityAvailability::Available,
        reason: None,
    });
    capabilities.push(CapabilityEntry {
        name: IAM_POLICY_ENTITIES_CAPABILITY.to_string(),
        availability: CapabilityAvailability::Available,
        reason: None,
    });
    capabilities.push(CapabilityEntry {
        name: IAM_POLICY_DETACH_CAPABILITY.to_string(),
        availability: CapabilityAvailability::Available,
        reason: None,
    });
    for name in [
        IAM_ACCESS_KEYS_BULK_CAPABILITY,
        IAM_ACCESS_KEYS_BULK_LDAP_CAPABILITY,
        IAM_ACCESS_KEYS_BULK_OPENID_CAPABILITY,
    ] {
        capabilities.push(CapabilityEntry {
            name: name.to_string(),
            availability: CapabilityAvailability::Available,
            reason: None,
        });
    }

    for (name, reason) in [
        (
            "admin.batch",
            "RustFS beta.10 registers batch routes without a scheduler or worker",
        ),
        (
            "admin.ldap-mutation",
            "RustFS beta.10 returns NotImplemented for LDAP mutations",
        ),
        (
            "admin.logs",
            "RustFS beta.10 exposes keepalive only and has no live log buffer",
        ),
        (
            "admin.trace",
            "RustFS beta.10 has no trace subscriber implementation",
        ),
        (
            "admin.update",
            "RustFS beta.10 accepts update requests without applying an update",
        ),
    ] {
        capabilities.push(CapabilityEntry {
            name: name.to_string(),
            availability: CapabilityAvailability::Stubbed,
            reason: Some(reason.to_string()),
        });
    }

    add_beta10_diagnostic_capabilities(capabilities);
}

fn add_uniform_diagnostic_capabilities(
    capabilities: &mut Vec<CapabilityEntry>,
    availability: CapabilityAvailability,
    reason: &str,
) {
    for capability in DiagnosticCapability::ALL {
        if capabilities
            .iter()
            .any(|entry| entry.name == capability.name())
        {
            continue;
        }
        capabilities.push(CapabilityEntry {
            name: capability.name().to_string(),
            availability,
            reason: Some(reason.to_string()),
        });
    }
}

fn add_beta10_diagnostic_capabilities(capabilities: &mut Vec<CapabilityEntry>) {
    for (capability, availability, reason) in [
        (
            DiagnosticCapability::HealthSnapshot,
            CapabilityAvailability::Available,
            "RustFS beta.10 provides a detailed health snapshot implementation",
        ),
        (
            DiagnosticCapability::DriveObservations,
            CapabilityAvailability::Available,
            "RustFS beta.10 provides drive storage observations",
        ),
        (
            DiagnosticCapability::ClientDevnull,
            CapabilityAvailability::Available,
            "RustFS beta.10 implements client devnull throughput measurement",
        ),
        (
            DiagnosticCapability::InspectArchive,
            CapabilityAvailability::Unsupported,
            "RustFS beta.10 inspect-data returns reconstructed object bytes, not an inspection archive",
        ),
        (
            DiagnosticCapability::ObjectSpeedtest,
            CapabilityAvailability::Stubbed,
            "RustFS beta.10 returns placeholder object speedtest results",
        ),
        (
            DiagnosticCapability::NetworkSpeedtest,
            CapabilityAvailability::Stubbed,
            "RustFS beta.10 returns placeholder network speedtest results",
        ),
        (
            DiagnosticCapability::SiteSpeedtest,
            CapabilityAvailability::Stubbed,
            "RustFS beta.10 returns placeholder site speedtest results",
        ),
        (
            DiagnosticCapability::SiteReplicationNetperf,
            CapabilityAvailability::Stubbed,
            "RustFS beta.10 site-replication netperf does not perform a peer network measurement",
        ),
    ] {
        if capabilities
            .iter()
            .any(|entry| entry.name == capability.name())
        {
            continue;
        }
        capabilities.push(CapabilityEntry {
            name: capability.name().to_string(),
            availability,
            reason: Some(reason.to_string()),
        });
    }

    mirror_read_only_diagnostic_routes(capabilities);
}

fn mirror_read_only_diagnostic_routes(capabilities: &mut Vec<CapabilityEntry>) {
    mirror_diagnostic_route(
        capabilities,
        DiagnosticCapability::ClusterSnapshot,
        "admin.cluster-snapshot-route",
    );
    mirror_diagnostic_route(
        capabilities,
        DiagnosticCapability::ExtensionsCatalog,
        "admin.extensions-catalog",
    );
}

fn mirror_diagnostic_route(
    capabilities: &mut Vec<CapabilityEntry>,
    diagnostic: DiagnosticCapability,
    route_capability_name: &str,
) {
    let route_state = capabilities
        .iter()
        .find(|capability| capability.name == route_capability_name)
        .map(|capability| (capability.availability, capability.reason.clone()));
    let (availability, reason) = route_state.unwrap_or_else(|| {
        (
            CapabilityAvailability::Unknown,
            Some("The diagnostic route was not classified during discovery".to_string()),
        )
    });
    capabilities.retain(|entry| entry.name != diagnostic.name());
    capabilities.push(CapabilityEntry {
        name: diagnostic.name().to_string(),
        availability,
        reason,
    });
}

fn diagnostic_route_error(error: Error, feature: &str) -> Error {
    match error {
        Error::NotFound(_) => {
            Error::UnsupportedFeature(format!("{feature} is unavailable on this RustFS server"))
        }
        error => error,
    }
}

fn is_rustfs_beta_10(version: &str) -> bool {
    version
        .split(|character: char| character.is_ascii_whitespace() || character == '/')
        .map(|component| component.trim_start_matches('v'))
        .any(|component| component.split('+').next() == Some("1.0.0-beta.10"))
}

#[derive(Debug, Deserialize)]
struct KmsServiceStatusResponse {
    status: serde_json::Value,
    #[serde(default)]
    backend_type: Option<String>,
    #[serde(default)]
    healthy: Option<bool>,
    #[serde(default)]
    config_summary: Option<KmsConfigSummaryResponse>,
}

#[derive(Debug, Deserialize)]
struct KmsConfigSummaryResponse {
    backend_type: String,
    #[serde(default)]
    default_key_id: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    retry_attempts: Option<u32>,
    #[serde(default)]
    enable_cache: bool,
    #[serde(default)]
    max_cached_keys: Option<u64>,
    #[serde(default)]
    cache_ttl_seconds: Option<u64>,
    #[serde(default)]
    cache_summary: Option<KmsCacheSummaryResponse>,
    #[serde(default)]
    backend_summary: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct KmsCacheSummaryResponse {
    #[serde(default)]
    max_keys: Option<u64>,
    #[serde(default)]
    ttl_seconds: Option<u64>,
    #[serde(default)]
    enable_metrics: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct KmsListKeysResponse {
    success: bool,
    message: String,
    #[serde(default)]
    keys: Vec<KmsKeyResponse>,
    #[serde(default)]
    truncated: bool,
    #[serde(default)]
    next_marker: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KmsDescribeKeyResponse {
    success: bool,
    message: String,
    #[serde(default)]
    key_metadata: Option<KmsKeyResponse>,
}

#[derive(Debug, Serialize)]
struct KmsCreateKeyBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    tags: BTreeMap<&'a str, &'a str>,
}

#[derive(Debug, Deserialize)]
struct KmsCreateKeyResponse {
    success: bool,
    message: String,
    #[serde(default)]
    key_id: Option<String>,
    #[serde(default)]
    key_metadata: Option<KmsKeyResponse>,
}

#[derive(Debug, Serialize)]
struct KmsDeleteKeyBody<'a> {
    key_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_window_in_days: Option<u32>,
    #[serde(skip_serializing_if = "is_false")]
    force_immediate: bool,
}

#[derive(Debug, Deserialize)]
struct KmsDeleteKeyResponse {
    success: bool,
    message: String,
    #[serde(default)]
    key_id: Option<String>,
    #[serde(default)]
    deletion_date: Option<String>,
}

#[derive(Debug, Serialize)]
struct KmsCancelKeyDeletionBody<'a> {
    key_id: &'a str,
}

#[derive(Debug, Deserialize)]
struct KmsCancelKeyDeletionResponse {
    success: bool,
    message: String,
    #[serde(default)]
    key_id: Option<String>,
    #[serde(default)]
    key_metadata: Option<KmsKeyResponse>,
}

#[derive(Debug, Serialize)]
struct KmsStartBody {
    force: bool,
}

#[derive(Deserialize)]
struct KmsLifecycleResponse {
    success: bool,
    message: String,
    status: serde_json::Value,
}

impl Drop for KmsLifecycleResponse {
    fn drop(&mut self) {
        self.message.zeroize();
        zeroize_json_value(&mut self.status);
    }
}

#[derive(Debug, Deserialize)]
struct KmsKeyResponse {
    key_id: String,
    #[serde(default, alias = "key_state", alias = "status")]
    state: Option<String>,
    #[serde(default, alias = "key_usage")]
    usage: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    algorithm: Option<String>,
    #[serde(default)]
    version: Option<u32>,
    #[serde(default, alias = "creation_date")]
    created_at: Option<String>,
    #[serde(default)]
    deletion_date: Option<String>,
    #[serde(default)]
    rotated_at: Option<String>,
    #[serde(default)]
    origin: Option<String>,
    #[serde(default, alias = "key_manager")]
    manager: Option<String>,
    #[serde(default)]
    tags: BTreeMap<String, String>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

impl From<KmsConfigSummaryResponse> for KmsConfigSummary {
    fn from(response: KmsConfigSummaryResponse) -> Self {
        let backend_details = response.backend_summary.as_object();
        let string_detail = |name: &str| {
            backend_details
                .and_then(|details| details.get(name))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let bool_detail = |name: &str| {
            backend_details
                .and_then(|details| details.get(name))
                .and_then(serde_json::Value::as_bool)
        };
        let cache = response.cache_summary;

        Self {
            backend: kms_backend_kind(&response.backend_type),
            default_key_id: response.default_key_id,
            timeout_seconds: response.timeout_seconds,
            retry_attempts: response.retry_attempts,
            cache: KmsCacheSummary {
                enabled: response.enable_cache,
                max_keys: cache
                    .as_ref()
                    .and_then(|summary| summary.max_keys)
                    .or(response.max_cached_keys),
                ttl_seconds: cache
                    .as_ref()
                    .and_then(|summary| summary.ttl_seconds)
                    .or(response.cache_ttl_seconds),
                metrics_enabled: cache.and_then(|summary| summary.enable_metrics),
            },
            endpoint: string_detail("address").and_then(|address| safe_backend_endpoint(&address)),
            auth_method: string_detail("auth_method_type"),
            credentials_configured: bool_detail("has_stored_credentials"),
            tls_verification_disabled: bool_detail("skip_tls_verify"),
        }
    }
}

impl From<KmsKeyResponse> for KmsKey {
    fn from(mut response: KmsKeyResponse) -> Self {
        response.metadata.append(&mut response.tags);
        Self {
            key_id: response.key_id,
            state: kms_key_state(response.state.as_deref()),
            usage: kms_key_usage(response.usage.as_deref()),
            description: response.description,
            algorithm: response.algorithm,
            version: response.version,
            created_at: response.created_at,
            deletion_date: response.deletion_date,
            rotated_at: response.rotated_at,
            origin: response.origin,
            manager: response.manager,
            tags: response.metadata,
        }
    }
}

fn kms_backend_kind(value: &str) -> KmsBackendKind {
    match value.trim().to_ascii_lowercase().as_str() {
        "local" => KmsBackendKind::Local,
        "vault" | "vaultkv2" | "vault-kv2" | "vault_kv2" => KmsBackendKind::VaultKv2,
        "vaulttransit" | "vault-transit" | "vault_transit" => KmsBackendKind::VaultTransit,
        _ => KmsBackendKind::Unknown,
    }
}

fn zeroize_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(value) => value.zeroize(),
        serde_json::Value::Array(values) => values.iter_mut().for_each(zeroize_json_value),
        serde_json::Value::Object(values) => {
            for (mut key, mut value) in std::mem::take(values) {
                key.zeroize();
                zeroize_json_value(&mut value);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn safe_backend_endpoint(value: &str) -> Option<String> {
    let mut endpoint = url::Url::parse(value).ok()?;
    endpoint.set_username("").ok()?;
    endpoint.set_password(None).ok()?;
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    Some(endpoint.into())
}

fn kms_service_state(value: &serde_json::Value) -> (KmsServiceState, Option<String>) {
    match value {
        serde_json::Value::String(state) => {
            let state = state.trim();
            let state = if ["notconfigured", "not-configured", "not_configured"]
                .iter()
                .any(|candidate| state.eq_ignore_ascii_case(candidate))
            {
                KmsServiceState::NotConfigured
            } else if state.eq_ignore_ascii_case("configured") {
                KmsServiceState::Configured
            } else if state.eq_ignore_ascii_case("running") {
                KmsServiceState::Running
            } else if state.eq_ignore_ascii_case("error") {
                KmsServiceState::Error
            } else {
                KmsServiceState::Unknown
            };
            (state, None)
        }
        serde_json::Value::Object(status) => status
            .get("Error")
            .or_else(|| status.get("error"))
            .and_then(serde_json::Value::as_str)
            .map_or((KmsServiceState::Unknown, None), |_| {
                (
                    KmsServiceState::Error,
                    Some("KMS service reported an error".to_string()),
                )
            }),
        _ => (KmsServiceState::Unknown, None),
    }
}

fn kms_key_state(value: Option<&str>) -> KmsKeyState {
    match value
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "enabled" => KmsKeyState::Enabled,
        "active" => KmsKeyState::Active,
        "disabled" => KmsKeyState::Disabled,
        "pendingdeletion" | "pending-deletion" | "pending_deletion" => KmsKeyState::PendingDeletion,
        "pendingimport" | "pending-import" | "pending_import" => KmsKeyState::PendingImport,
        "unavailable" => KmsKeyState::Unavailable,
        "deleted" => KmsKeyState::Deleted,
        _ => KmsKeyState::Unknown,
    }
}

fn kms_key_usage(value: Option<&str>) -> KmsKeyUsage {
    match value
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "encryptdecrypt" | "encrypt-decrypt" | "encrypt_decrypt" => KmsKeyUsage::EncryptDecrypt,
        "signverify" | "sign-verify" | "sign_verify" => KmsKeyUsage::SignVerify,
        _ => KmsKeyUsage::Unknown,
    }
}

fn kms_mutation_error(operation: &str, message: &str) -> Error {
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("already exists") || normalized.contains("conflict") {
        Error::Conflict(format!("KMS key {operation} conflicts with existing state"))
    } else if normalized.contains("not found") || normalized.contains("does not exist") {
        Error::NotFound(format!("KMS key {operation} target was not found"))
    } else if normalized.contains("not initialized")
        || normalized.contains("not running")
        || normalized.contains("unavailable")
    {
        Error::Network("KMS service is unavailable".to_string())
    } else {
        Error::General(format!("KMS key {operation} request was rejected"))
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

fn sanitize_kms_mutation_error(operation: &str, error: Error) -> Error {
    match error {
        Error::Auth(_) => Error::Auth("KMS mutation permission was denied".to_string()),
        Error::NotFound(_) => Error::NotFound(format!("KMS key {operation} target was not found")),
        Error::Conflict(_) => {
            Error::Conflict(format!("KMS key {operation} conflicts with existing state"))
        }
        Error::Network(diagnostic) => {
            let classified = kms_mutation_error(operation, &diagnostic);
            match classified {
                Error::Conflict(_) | Error::NotFound(_) => classified,
                _ => Error::Network("KMS service is unavailable".to_string()),
            }
        }
        Error::Json(error) => Error::Json(error),
        Error::InvalidPath(_) | Error::Config(_) => error,
        error => kms_mutation_error(operation, &error.to_string()),
    }
}

fn kms_lifecycle_error(operation: &str, message: &str) -> Error {
    if contains_ascii_case_insensitive(message, "not configured")
        || contains_ascii_case_insensitive(message, "unconfigured")
    {
        Error::NotFound("KMS service is not configured".to_string())
    } else if contains_ascii_case_insensitive(message, "already running")
        || contains_ascii_case_insensitive(message, "already stopped")
        || contains_ascii_case_insensitive(message, "conflict")
    {
        Error::Conflict(format!(
            "KMS {operation} conflicts with current service state"
        ))
    } else if contains_ascii_case_insensitive(message, "not initialized")
        || contains_ascii_case_insensitive(message, "unavailable")
        || contains_ascii_case_insensitive(message, "storage layer not initialized")
    {
        Error::Network("KMS service is unavailable".to_string())
    } else {
        Error::General(format!("KMS {operation} request was rejected"))
    }
}

fn contains_ascii_case_insensitive(value: &str, needle: &str) -> bool {
    value
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn kms_lifecycle_status_error(status: StatusCode) -> Error {
    match status {
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => {
            Error::Auth("KMS administration permission was denied".to_string())
        }
        StatusCode::NOT_FOUND | StatusCode::NOT_IMPLEMENTED => Error::UnsupportedFeature(
            "The RustFS KMS administration route is unavailable".to_string(),
        ),
        StatusCode::CONFLICT => {
            Error::Conflict("KMS operation conflicts with current service state".to_string())
        }
        StatusCode::BAD_REQUEST => {
            Error::General("KMS administration request was rejected".to_string())
        }
        _ => Error::Network("KMS service is unavailable".to_string()),
    }
}

fn sanitize_kms_lifecycle_error(operation: &str, error: Error) -> Error {
    match error {
        Error::Auth(_) => Error::Auth("KMS administration permission was denied".to_string()),
        Error::NotFound(_) => Error::NotFound("KMS service is not configured".to_string()),
        Error::Conflict(_) => Error::Conflict(format!(
            "KMS {operation} conflicts with current service state"
        )),
        Error::Network(mut diagnostic) => {
            let classified = kms_lifecycle_error(operation, &diagnostic);
            diagnostic.zeroize();
            match classified {
                Error::NotFound(_) | Error::Conflict(_) => classified,
                _ => Error::Network("KMS service is unavailable".to_string()),
            }
        }
        Error::Json(error) => Error::Json(error),
        Error::UnsupportedFeature(_) => Error::UnsupportedFeature(
            "The RustFS KMS administration route is unavailable".to_string(),
        ),
        Error::InvalidPath(_) | Error::Config(_) => error,
        _ => Error::General(format!("KMS {operation} request failed")),
    }
}

fn kms_lifecycle_state(operation: &str, response: KmsLifecycleResponse) -> Result<KmsServiceState> {
    if !response.success {
        return Err(kms_lifecycle_error(operation, &response.message));
    }
    Ok(kms_service_state(&response.status).0)
}

#[async_trait]
impl KmsApi for AdminClient {
    async fn kms_status(&self) -> Result<KmsStatus> {
        let response: KmsServiceStatusResponse = self
            .request(Method::GET, "/kms/service-status", None, None)
            .await?;
        let (state, error_message) = kms_service_state(&response.status);
        let backend = response.backend_type.as_deref().map(kms_backend_kind);
        Ok(KmsStatus {
            state,
            backend,
            healthy: response.healthy,
            error_message,
            config: response.config_summary.map(Into::into),
        })
    }

    async fn kms_list_keys(&self, limit: u32, marker: Option<&str>) -> Result<KmsKeyPage> {
        let limit = limit.to_string();
        let mut query = vec![("limit", limit.as_str())];
        if let Some(marker) = marker {
            query.push(("marker", marker));
        }
        let response: KmsListKeysResponse = self
            .request(Method::GET, "/kms/keys", Some(&query), None)
            .await?;
        if !response.success {
            return Err(Error::General(response.message));
        }
        Ok(KmsKeyPage {
            keys: response.keys.into_iter().map(Into::into).collect(),
            truncated: response.truncated,
            next_marker: response.next_marker,
        })
    }

    async fn kms_describe_key(&self, key_id: &str) -> Result<KmsKey> {
        if key_id.trim().is_empty() {
            return Err(Error::InvalidPath("KMS key id cannot be empty".to_string()));
        }
        let path = format!("/kms/keys/{}", urlencoding::encode(key_id));
        let response: KmsDescribeKeyResponse = self.request(Method::GET, &path, None, None).await?;
        if !response.success {
            return Err(Error::General(response.message));
        }
        response
            .key_metadata
            .map(Into::into)
            .ok_or_else(|| Error::Network("KMS describe response omitted key metadata".to_string()))
    }

    async fn kms_create_key(&self, request: &KmsCreateKeyRequest) -> Result<KmsCreateKeyResult> {
        if request.tags.contains_key("name") {
            return Err(Error::InvalidPath(
                "The KMS tag key 'name' is reserved; use the name field".to_string(),
            ));
        }
        let mut tags = request
            .tags
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<BTreeMap<_, _>>();
        if let Some(name) = request.name.as_deref() {
            tags.insert("name", name);
        }
        let body = serde_json::to_vec(&KmsCreateKeyBody {
            description: request.description.as_deref(),
            tags,
        })
        .map_err(Error::Json)?;
        let response: KmsCreateKeyResponse = self
            .request(Method::POST, "/kms/keys", None, Some(&body))
            .await
            .map_err(|error| sanitize_kms_mutation_error("creation", error))?;
        if !response.success {
            return Err(kms_mutation_error("creation", &response.message));
        }
        let key = response.key_metadata.map(Into::into);
        let key_id = response
            .key_id
            .or_else(|| key.as_ref().map(|key: &KmsKey| key.key_id.clone()))
            .ok_or_else(|| {
                Error::Network("KMS create response omitted the key identifier".to_string())
            })?;
        Ok(KmsCreateKeyResult { key_id, key })
    }

    async fn kms_delete_key(&self, request: &KmsDeleteKeyRequest) -> Result<KmsDeleteKeyResult> {
        if request.key_id.trim().is_empty() {
            return Err(Error::InvalidPath("KMS key id cannot be empty".to_string()));
        }
        if request.force_immediate && request.pending_window_in_days.is_some() {
            return Err(Error::InvalidPath(
                "Immediate KMS deletion cannot include a pending window".to_string(),
            ));
        }
        if request
            .pending_window_in_days
            .is_some_and(|days| !(7..=30).contains(&days))
        {
            return Err(Error::InvalidPath(
                "KMS deletion pending window must be between 7 and 30 days".to_string(),
            ));
        }
        let body = serde_json::to_vec(&KmsDeleteKeyBody {
            key_id: &request.key_id,
            pending_window_in_days: request.pending_window_in_days,
            force_immediate: request.force_immediate,
        })
        .map_err(Error::Json)?;
        let response: KmsDeleteKeyResponse = self
            .request(Method::DELETE, "/kms/keys/delete", None, Some(&body))
            .await
            .map_err(|error| sanitize_kms_mutation_error("deletion", error))?;
        if !response.success {
            return Err(kms_mutation_error("deletion", &response.message));
        }
        let key_id = response.key_id.ok_or_else(|| {
            Error::Network("KMS delete response omitted the key identifier".to_string())
        })?;
        Ok(KmsDeleteKeyResult {
            key_id,
            deletion_date: response.deletion_date,
            immediate: request.force_immediate,
        })
    }

    async fn kms_cancel_key_deletion(&self, key_id: &str) -> Result<KmsCancelKeyDeletionResult> {
        if key_id.trim().is_empty() {
            return Err(Error::InvalidPath("KMS key id cannot be empty".to_string()));
        }
        let body = serde_json::to_vec(&KmsCancelKeyDeletionBody { key_id }).map_err(Error::Json)?;
        let response: KmsCancelKeyDeletionResponse = self
            .request(Method::POST, "/kms/keys/cancel-deletion", None, Some(&body))
            .await
            .map_err(|error| sanitize_kms_mutation_error("deletion cancellation", error))?;
        if !response.success {
            return Err(kms_mutation_error(
                "deletion cancellation",
                &response.message,
            ));
        }
        let key = response.key_metadata.map(Into::into);
        let key_id = response
            .key_id
            .or_else(|| key.as_ref().map(|key: &KmsKey| key.key_id.clone()))
            .ok_or_else(|| {
                Error::Network("KMS cancellation response omitted the key identifier".to_string())
            })?;
        Ok(KmsCancelKeyDeletionResult { key_id, key })
    }

    async fn kms_configure(&self, request: &KmsConfigureRequest) -> Result<KmsServiceState> {
        request.validate(false)?;
        self.kms_send_configuration("configure", "/kms/configure", request)
            .await
    }

    async fn kms_reconfigure(&self, request: &KmsConfigureRequest) -> Result<KmsServiceState> {
        request.validate(true)?;
        self.kms_send_configuration("reconfiguration", "/kms/reconfigure", request)
            .await
    }

    async fn kms_start(&self, force: bool) -> Result<KmsServiceState> {
        let body = Zeroizing::new(serde_json::to_vec(&KmsStartBody { force }).map_err(|_| {
            Error::General("Failed to encode KMS service control request".to_string())
        })?);
        let response: KmsLifecycleResponse = self
            .request_sensitive(Method::POST, "/kms/start", Some(body))
            .await
            .map_err(|error| {
                sanitize_kms_lifecycle_error(if force { "restart" } else { "start" }, error)
            })?;
        kms_lifecycle_state(if force { "restart" } else { "start" }, response)
    }

    async fn kms_stop(&self) -> Result<KmsServiceState> {
        let response: KmsLifecycleResponse = self
            .request_sensitive(Method::POST, "/kms/stop", None)
            .await
            .map_err(|error| sanitize_kms_lifecycle_error("stop", error))?;
        kms_lifecycle_state("stop", response)
    }
}

impl AdminClient {
    async fn kms_send_configuration(
        &self,
        operation: &str,
        path: &str,
        request: &KmsConfigureRequest,
    ) -> Result<KmsServiceState> {
        let body = Zeroizing::new(serde_json::to_vec(request).map_err(|_| {
            Error::General("Failed to encode KMS configuration request".to_string())
        })?);
        let response: KmsLifecycleResponse = self
            .request_sensitive(Method::POST, path, Some(body))
            .await
            .map_err(|error| sanitize_kms_lifecycle_error(operation, error))?;
        kms_lifecycle_state(operation, response)
    }
}

#[async_trait]
impl CapabilityApi for AdminClient {
    async fn discover_capabilities(&self, refresh: bool) -> Result<CapabilityReport> {
        if !refresh && let Some(report) = self.cached_capabilities()? {
            return Ok(report);
        }

        let report = self.discover_capabilities_uncached().await?;
        self.store_capabilities(&report)?;
        Ok(report)
    }
}

#[async_trait]
impl OidcReadApi for AdminClient {
    async fn oidc_list_providers(&self) -> Result<OidcProviderList> {
        let response: OidcProviderList =
            self.request_oidc(Method::GET, "/oidc/config", None).await?;
        response.validate_response().map_err(|_| {
            Error::General("RustFS returned an invalid OIDC provider list".to_string())
        })?;
        Ok(response)
    }

    async fn oidc_get_provider(&self, provider_id: &str) -> Result<OidcProvider> {
        if provider_id.is_empty()
            || !provider_id.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            })
        {
            return Err(Error::InvalidPath(
                "OIDC provider ID must contain only ASCII letters, digits, '_' or '-'".to_string(),
            ));
        }
        self.oidc_list_providers()
            .await?
            .providers
            .into_iter()
            .find(|provider| provider.provider_id == provider_id)
            .ok_or_else(|| Error::NotFound(format!("OIDC provider '{provider_id}' was not found")))
    }

    async fn oidc_validate(&self, request: OidcValidationRequest) -> Result<OidcValidationResult> {
        request.validate()?;
        let body = serde_json::to_vec(&request)
            .map_err(|_| Error::General("Failed to encode OIDC validation request".to_string()))?;
        let response: OidcValidationResult = self
            .request_oidc(Method::POST, "/oidc/validate", Some(&body))
            .await?;
        response.validate_response().map_err(|_| {
            Error::General("RustFS returned an invalid OIDC validation response".to_string())
        })?;
        Ok(response)
    }
}

#[async_trait]
impl IamReadApi for AdminClient {
    async fn policy_entities(&self, query: &PolicyEntitiesQuery) -> Result<PolicyEntitiesResult> {
        query.validate()?;

        let mut parameters =
            Vec::with_capacity(query.users.len() + query.groups.len() + query.policies.len());
        parameters.extend(query.users.iter().map(|value| ("user", value.as_str())));
        parameters.extend(query.groups.iter().map(|value| ("group", value.as_str())));
        parameters.extend(
            query
                .policies
                .iter()
                .map(|value| ("policy", value.as_str())),
        );
        let query = (!parameters.is_empty()).then_some(parameters.as_slice());

        let result: PolicyEntitiesResult = self
            .request_bounded_json(
                Method::GET,
                "/idp/builtin/policy-entities",
                query,
                None,
                BoundedJsonResponse {
                    max_bytes: MAX_IAM_POLICY_ENTITIES_RESPONSE_BYTES,
                    name: "IAM policy-entity response",
                    error_mapper: Self::map_iam_policy_entities_error,
                },
            )
            .await?;
        result.normalize()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BulkAccessKeyWireEntry {
    access_key: String,
    #[serde(default)]
    parent_user: String,
    #[serde(default)]
    account_status: Option<String>,
    #[serde(default)]
    expiration: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    implied_policy: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BulkAccessKeyWireGroup {
    #[serde(default)]
    service_accounts: Vec<BulkAccessKeyWireEntry>,
    #[serde(default)]
    sts_keys: Vec<BulkAccessKeyWireEntry>,
}

#[async_trait]
impl BulkAccessKeyApi for AdminClient {
    async fn list_access_keys_bulk(
        &self,
        query: &BulkAccessKeyQuery,
    ) -> Result<Vec<AccessKeyRecord>> {
        query.validate()?;

        let mut parameters = Vec::with_capacity(query.users.len() + 2);
        parameters.extend(query.users.iter().map(|user| ("users", user.as_str())));
        if query.all {
            parameters.push(("all", "true"));
        }
        parameters.push(("listType", query.list_type.wire_value()));

        let path = match query.provider {
            AccessKeyProvider::Builtin => "/list-access-keys-bulk",
            AccessKeyProvider::Ldap => "/idp/ldap/list-access-keys-bulk",
            AccessKeyProvider::Openid => "/idp/openid/list-access-keys-bulk",
        };
        let response: BTreeMap<String, BulkAccessKeyWireGroup> = self
            .request_bounded_json(
                Method::GET,
                path,
                Some(parameters.as_slice()),
                None,
                BoundedJsonResponse {
                    max_bytes: MAX_IAM_ACCESS_KEYS_RESPONSE_BYTES,
                    name: "bulk access-key response",
                    error_mapper: Self::map_iam_access_keys_error,
                },
            )
            .await
            .map_err(|error| match error {
                Error::Json(_) => Error::General(
                    "RustFS returned a malformed bulk access-key response".to_string(),
                ),
                other => other,
            })?;

        normalize_access_key_response(response, query.provider)
    }
}

fn normalize_access_key_response(
    response: BTreeMap<String, BulkAccessKeyWireGroup>,
    provider: AccessKeyProvider,
) -> Result<Vec<AccessKeyRecord>> {
    let count = response.values().fold(0_usize, |count, group| {
        count
            .saturating_add(group.service_accounts.len())
            .saturating_add(group.sts_keys.len())
    });
    if count > MAX_IAM_ACCESS_KEY_RESULTS {
        return Err(Error::General(format!(
            "RustFS bulk access-key response exceeds the {MAX_IAM_ACCESS_KEY_RESULTS} item limit"
        )));
    }

    let mut records = Vec::with_capacity(count);
    for (parent, group) in response {
        for (kind, entries) in [
            (AccessKeyKind::ServiceAccount, group.service_accounts),
            (AccessKeyKind::Sts, group.sts_keys),
        ] {
            for entry in entries {
                if entry.access_key.trim().is_empty() {
                    return Err(Error::General(
                        "RustFS bulk access-key response contains an empty access key".to_string(),
                    ));
                }
                let parent_user = if entry.parent_user.trim().is_empty() {
                    parent.clone()
                } else {
                    entry.parent_user
                };
                if parent_user.trim().is_empty() {
                    return Err(Error::General(
                        "RustFS bulk access-key response contains an empty parent identity"
                            .to_string(),
                    ));
                }
                records.push(AccessKeyRecord {
                    access_key: entry.access_key,
                    kind,
                    provider,
                    parent_user,
                    account_status: entry.account_status,
                    expiration: entry.expiration,
                    name: entry.name,
                    description: entry.description,
                    implied_policy: entry.implied_policy,
                });
            }
        }
    }
    records.sort_by(|left, right| {
        (
            left.provider,
            left.parent_user.as_str(),
            left.kind,
            left.access_key.as_str(),
        )
            .cmp(&(
                right.provider,
                right.parent_user.as_str(),
                right.kind,
                right.access_key.as_str(),
            ))
    });
    records.dedup_by(|left, right| {
        left.provider == right.provider
            && left.parent_user == right.parent_user
            && left.kind == right.kind
            && left.access_key == right.access_key
    });
    Ok(records)
}

#[async_trait]
impl OidcMutationApi for AdminClient {
    async fn oidc_upsert_provider(
        &self,
        request: OidcMutationRequest,
    ) -> Result<OidcMutationResult> {
        request.validate()?;
        let provider_id = request.provider_id.clone();
        let body = Zeroizing::new(serde_json::to_vec(&request).map_err(|_| {
            Error::General("Failed to encode OIDC configuration request".to_string())
        })?);
        let path = format!("/oidc/config/{}", urlencoding::encode(&provider_id));
        let response: OidcMutationResult = self.request_oidc_mutation(&path, body).await?;
        response.validate_response().map_err(|_| {
            Error::General("RustFS returned an invalid OIDC mutation response".to_string())
        })?;
        Ok(response)
    }

    async fn oidc_delete_provider(&self, provider_id: &str) -> Result<OidcMutationResult> {
        if provider_id.is_empty()
            || !provider_id.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            })
        {
            return Err(Error::InvalidPath(
                "OIDC provider ID must contain only ASCII letters, digits, '_' or '-'".to_string(),
            ));
        }
        let path = format!("/oidc/config/{}", urlencoding::encode(provider_id));
        let response: OidcMutationResult = self.request_oidc_delete(&path).await?;
        response.validate_response().map_err(|_| {
            Error::General("RustFS returned an invalid OIDC deletion response".to_string())
        })?;
        Ok(response)
    }
}

#[async_trait]
impl IamMutationApi for AdminClient {
    async fn detach_policies(&self, request: &PolicyDetachRequest) -> Result<PolicyDetachResult> {
        let request = PolicyDetachRequest::new(
            request.policies.clone(),
            request.entity,
            request.entity_name.clone(),
        )?;
        let wire = PolicyDetachWireRequest {
            policies: &request.policies,
            user: (request.entity == PolicyDetachEntity::User)
                .then_some(request.entity_name.as_str()),
            group: (request.entity == PolicyDetachEntity::Group)
                .then_some(request.entity_name.as_str()),
        };
        let body = serde_json::to_vec(&wire).map_err(Error::Json)?;
        if body.len() > MAX_IAM_POLICY_DETACH_REQUEST_BYTES {
            return Err(Error::InvalidPath(format!(
                "Policy detach request exceeds the {MAX_IAM_POLICY_DETACH_REQUEST_BYTES}-byte limit"
            )));
        }
        let response: PolicyDetachWireResponse = self
            .request_bounded_json(
                Method::POST,
                "/idp/builtin/policy/detach",
                None,
                Some(&body),
                BoundedJsonResponse {
                    max_bytes: MAX_IAM_POLICY_DETACH_RESPONSE_BYTES,
                    name: "IAM policy detach response",
                    error_mapper: Self::map_iam_policy_detach_error,
                },
            )
            .await
            .map_err(|error| match error {
                Error::Json(_) => {
                    Error::General("RustFS returned a malformed policy detach response".to_string())
                }
                other => other,
            })?;

        let mut attached = response.policies_attached;
        let mut detached = response.policies_detached;
        normalize_policy_detach_response_names("attached", &mut attached)?;
        normalize_policy_detach_response_names("detached", &mut detached)?;
        if attached
            .iter()
            .any(|policy| !request.policies.contains(policy))
            || detached
                .iter()
                .any(|policy| !request.policies.contains(policy))
        {
            return Err(Error::General(
                "RustFS policy detach response contains an unrequested policy".to_string(),
            ));
        }
        if attached.iter().any(|policy| detached.contains(policy)) {
            return Err(Error::General(
                "RustFS policy detach response reports a policy as both attached and detached"
                    .to_string(),
            ));
        }

        let unchanged = request
            .policies
            .iter()
            .filter(|policy| !detached.contains(policy))
            .cloned()
            .collect();
        Ok(PolicyDetachResult {
            entity: request.entity,
            entity_name: request.entity_name,
            attached,
            detached,
            unchanged,
            updated_at: response.updated_at,
        })
    }
}

fn normalize_policy_detach_response_names(kind: &str, names: &mut Vec<String>) -> Result<()> {
    if names.iter().any(|name| name.trim().is_empty()) {
        return Err(Error::General(format!(
            "RustFS policy detach response contains an empty {kind} policy"
        )));
    }
    names.sort();
    names.dedup();
    Ok(())
}

#[async_trait]
impl BucketMetadataApi for AdminClient {
    async fn export_bucket_metadata(&self, bucket: Option<&str>) -> Result<BucketMetadataArchive> {
        let mut url = self.admin_url("/export-bucket-metadata");
        if let Some(bucket) = bucket {
            url.push_str("?bucket=");
            url.push_str(&urlencoding::encode(bucket));
        }
        let headers = self.request_headers(&[])?;
        let signed_headers = self.sign_request(&Method::GET, &url, &headers, &[]).await?;
        let mut request = self.http_client.get(&url);
        for (name, value) in &signed_headers {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|_| Error::Network("Bucket metadata export request failed".to_string()))?;
        let status = response.status();
        let bytes = read_bounded_response_body(
            response,
            MAX_BUCKET_METADATA_ARCHIVE_BYTES,
            "Bucket metadata export response",
        )
        .await?;
        if !status.is_success() {
            return Err(bucket_metadata_status_error(status, false));
        }
        BucketMetadataArchive::new(bytes)
    }

    async fn import_bucket_metadata(&self, archive: BucketMetadataArchive) -> Result<()> {
        let body = archive.into_bytes();
        let url = self.admin_url("/import-bucket-metadata");
        let mut headers = self.request_headers(body.as_slice())?;
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/zip"));
        let signed_headers = self
            .sign_request(&Method::PUT, &url, &headers, body.as_slice())
            .await?;
        let mut request = self.http_client.put(&url);
        for (name, value) in &signed_headers {
            request = request.header(name, value);
        }
        let response = request
            .body(Bytes::from_owner(SensitiveRequestBody(body)))
            .send()
            .await
            .map_err(|_| {
                Error::Network(
                    "Bucket metadata import outcome is unknown; the mutation was not retried"
                        .to_string(),
                )
            })?;
        let status = response.status();
        let response_body = read_bounded_response_body(
            response,
            MAX_BUCKET_METADATA_MUTATION_RESPONSE_BYTES,
            "Bucket metadata import response",
        )
        .await
        .map_err(|_| {
            Error::Network(
                "Bucket metadata import outcome is unknown; inspect every selected bucket before retrying"
                    .to_string(),
            )
        })?;
        if !status.is_success() {
            return Err(bucket_metadata_status_error(status, true));
        }
        if !response_body.is_empty()
            && serde_json::from_slice::<serde_json::Value>(&response_body).is_err()
        {
            return Err(Error::Network(
                "Bucket metadata import returned an unexpected response; inspect every selected bucket before retrying"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

fn bucket_metadata_status_error(status: StatusCode, mutation: bool) -> Error {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            Error::Auth("Bucket metadata administration permission was denied".to_string())
        }
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED => {
            Error::UnsupportedFeature(
                "The RustFS bucket metadata archive route is unavailable".to_string(),
            )
        }
        StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => {
            Error::Conflict("Bucket metadata changed concurrently".to_string())
        }
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
            Error::Config("RustFS rejected the bucket metadata archive".to_string())
        }
        _ if mutation => Error::Network(
            "Bucket metadata import may be partially applied; inspect every selected bucket before retrying"
                .to_string(),
        ),
        _ => Error::Network("Bucket metadata export service is unavailable".to_string()),
    }
}

#[async_trait]
impl IamArchiveApi for AdminClient {
    async fn export_iam_archive(&self) -> Result<Vec<u8>> {
        let url = self.admin_url("/export-iam");
        let headers = self.request_headers(&[])?;
        let signed_headers = self.sign_request(&Method::GET, &url, &headers, &[]).await?;
        let mut request = self.http_client.request(Method::GET, &url);
        for (name, value) in &signed_headers {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|_| Error::Network("IAM export request failed".to_string()))?;
        let status = response.status();
        let body =
            read_bounded_response_body(response, MAX_IAM_ARCHIVE_BYTES, "IAM export archive")
                .await?;
        if !status.is_success() {
            return Err(iam_archive_status_error(status, false));
        }
        Ok(body)
    }

    async fn import_iam_archive(&self, archive: Vec<u8>) -> Result<IamArchiveImportResult> {
        if archive.len() > MAX_IAM_ARCHIVE_BYTES {
            return Err(Error::RequestRejected(format!(
                "IAM archive exceeds the {MAX_IAM_ARCHIVE_BYTES} byte limit"
            )));
        }
        let archive = Zeroizing::new(archive);
        let url = self.admin_url("/import-iam-v2");
        let mut headers = self.request_headers(&archive)?;
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/zip"));
        let signed_headers = self
            .sign_request(&Method::PUT, &url, &headers, &archive)
            .await?;
        let mut request = self.http_client.request(Method::PUT, &url);
        for (name, value) in &signed_headers {
            request = request.header(name, value);
        }
        request = request.body(Bytes::from_owner(SensitiveRequestBody(archive)));
        let response = request.send().await.map_err(|_| {
            Error::Network(
                "IAM import outcome is unknown; the mutation was not retried".to_string(),
            )
        })?;
        let status = response.status();
        let body = read_bounded_response_body(
            response,
            MAX_IAM_IMPORT_RESPONSE_BYTES,
            "IAM import response",
        )
        .await
        .map_err(|_| {
            Error::General(
                "IAM import outcome is unknown because its response was rejected; inspect destination IAM state before retrying"
                    .to_string(),
            )
        })?;
        if !status.is_success() {
            return Err(iam_archive_status_error(status, true));
        }
        let result: WireIamImportResult = serde_json::from_slice(&body).map_err(|_| {
            Error::General(
                "IAM import outcome is unknown because RustFS returned a malformed response; inspect destination IAM state before retrying"
                    .to_string(),
            )
        })?;
        Ok(result.into_safe_result())
    }

    async fn iam_archive_conflicts(
        &self,
        inventory: &IamArchiveInventory,
    ) -> Result<IamArchiveInventory> {
        let users = self
            .list_users()
            .await
            .map_err(sanitize_iam_preflight_error)?
            .into_iter()
            .map(|entry| entry.access_key)
            .collect::<std::collections::BTreeSet<_>>();
        let groups = self
            .list_groups()
            .await
            .map_err(sanitize_iam_preflight_error)?
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let policies = self
            .list_policies()
            .await
            .map_err(sanitize_iam_preflight_error)?
            .into_iter()
            .map(|entry| entry.name)
            .collect::<std::collections::BTreeSet<_>>();
        let service_accounts = self
            .list_service_accounts(None)
            .await
            .map_err(sanitize_iam_preflight_error)?
            .into_iter()
            .map(|entry| entry.access_key)
            .collect::<std::collections::BTreeSet<_>>();
        Ok(IamArchiveInventory {
            users: inventory
                .users
                .iter()
                .filter(|name| users.contains(*name))
                .cloned()
                .collect(),
            groups: inventory
                .groups
                .iter()
                .filter(|name| groups.contains(*name))
                .cloned()
                .collect(),
            policies: inventory
                .policies
                .iter()
                .filter(|name| policies.contains(*name))
                .cloned()
                .collect(),
            service_accounts: inventory
                .service_accounts
                .iter()
                .filter(|name| service_accounts.contains(*name))
                .cloned()
                .collect(),
        })
    }
}

fn iam_archive_status_error(status: StatusCode, mutation: bool) -> Error {
    let operation = if mutation { "import" } else { "export" };
    match status {
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => {
            Error::Auth(format!("Permission denied for IAM {operation}"))
        }
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED => {
            Error::UnsupportedFeature(format!(
                "IAM {operation} is not supported by this RustFS server"
            ))
        }
        StatusCode::CONFLICT => {
            Error::Conflict("IAM import conflicts with destination state".to_string())
        }
        StatusCode::BAD_REQUEST => {
            Error::General(format!("RustFS rejected the IAM {operation} request"))
        }
        _ if mutation => Error::General(format!(
            "IAM import outcome is unknown after HTTP {}; inspect destination IAM state before retrying",
            status.as_u16()
        )),
        _ => Error::Network(format!("IAM export failed with HTTP {}", status.as_u16())),
    }
}

fn sanitize_iam_preflight_error(error: Error) -> Error {
    match error {
        Error::Auth(_) => {
            Error::Auth("Permission denied during IAM conflict preflight".to_string())
        }
        Error::NotFound(_) | Error::UnsupportedFeature(_) => Error::UnsupportedFeature(
            "IAM conflict preflight is not supported by this RustFS server".to_string(),
        ),
        Error::Network(_) => Error::Network("IAM conflict preflight request failed".to_string()),
        Error::Conflict(_) => {
            Error::Conflict("IAM conflict preflight could not read destination state".to_string())
        }
        _ => Error::General("IAM conflict preflight returned an invalid response".to_string()),
    }
}

#[async_trait]
impl DiagnosticReadApi for AdminClient {
    async fn health_snapshot(&self) -> Result<DetailedHealthSnapshot> {
        self.request_bounded_json_url(Method::GET, self.admin_url("/healthinfo"))
            .await
            .map_err(|error| diagnostic_route_error(error, "Detailed health snapshot"))
    }

    async fn cluster_snapshot(&self) -> Result<ClusterSnapshotDocument> {
        self.request_bounded_json_url(Method::GET, self.admin_v4_url("/cluster/snapshot"))
            .await
            .map_err(|error| diagnostic_route_error(error, "Cluster snapshot"))
    }

    async fn extensions_catalog(&self) -> Result<ExtensionsCatalog> {
        self.request_bounded_json_url(Method::GET, self.admin_v4_url("/extensions/catalog"))
            .await
            .map_err(|error| diagnostic_route_error(error, "Extension catalog"))
    }
}

#[async_trait]
impl InspectArchiveApi for AdminClient {
    async fn inspect_archive_capability(&self) -> Result<InspectArchiveCapabilityContract> {
        let runtime = self
            .request_v4::<RuntimeCapabilitiesSnapshot>(Method::GET, "/runtime/capabilities")
            .await
            .map_err(|error| diagnostic_route_error(error, "Diagnostic archive capability"))?;
        runtime.inspect_archive.ok_or_else(|| {
            Error::UnsupportedFeature(
                "RustFS did not advertise an encrypted diagnostic archive contract".to_string(),
            )
        })
    }

    async fn download_inspect_archive(
        &self,
        request: InspectArchiveTransportRequest,
        temporary_directory: &std::path::Path,
    ) -> Result<EncryptedInspectArchive> {
        let operation = async {
            let body = serde_json::to_vec(&InspectArchiveWireRequest {
                bucket: &request.bucket,
                object: &request.object,
                public_key_pem: &request.public_key_pem,
            })
            .map_err(|_| {
                Error::General("Failed to encode diagnostic archive request".to_string())
            })?;
            let url = self.admin_v4_url("/inspect/archive");
            let headers = self.request_headers(&body)?;
            let signed_headers = self
                .sign_request(&Method::POST, &url, &headers, &body)
                .await?;
            let mut builder = self
                .http_client
                .request(Method::POST, &url)
                .header(CONTENT_TYPE, "application/json")
                .body(body);
            for (name, value) in &signed_headers {
                builder = builder.header(name, value);
            }
            let response = builder
                .send()
                .await
                .map_err(|_| Error::Network("Diagnostic archive request failed".to_string()))?;
            let status = response.status();
            if !status.is_success() {
                let body = read_bounded_response_body(
                    response,
                    64 * 1024,
                    "Diagnostic archive error response",
                )
                .await?;
                return Err(self.map_inspect_archive_error(status, &String::from_utf8_lossy(&body)));
            }
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok());
            let completion = response
                .headers()
                .get("x-rustfs-inspect-completion")
                .and_then(|value| value.to_str().ok());
            let cache_control = response
                .headers()
                .get(CACHE_CONTROL)
                .and_then(|value| value.to_str().ok());
            if content_type != Some(INSPECT_ARCHIVE_CONTENT_TYPE)
                || completion != Some(INSPECT_ARCHIVE_COMPLETION)
                || !cache_control.is_some_and(|value| {
                    value
                        .split(',')
                        .any(|directive| directive.trim().eq_ignore_ascii_case("no-store"))
                })
            {
                return Err(Error::General(
                    "Diagnostic archive response headers do not match the advertised contract"
                        .to_string(),
                ));
            }
            if response
                .content_length()
                .is_some_and(|length| length > request.max_bytes as u64)
            {
                return Err(Error::RequestRejected(
                    "Diagnostic archive response exceeds the advertised byte limit".to_string(),
                ));
            }

            let temporary = tempfile::Builder::new()
                .prefix(".rc-inspect-encrypted-")
                .tempfile_in(temporary_directory)
                .map_err(|_| {
                    Error::InvalidPath(
                        "Failed to create private encrypted archive staging file".to_string(),
                    )
                })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                temporary
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(0o600))
                    .map_err(|_| {
                        Error::InvalidPath(
                            "Failed to protect encrypted archive staging file".to_string(),
                        )
                    })?;
            }
            let file = temporary.reopen().map_err(|_| {
                Error::InvalidPath(
                    "Failed to open private encrypted archive staging file".to_string(),
                )
            })?;
            let mut file = tokio::fs::File::from_std(file);
            let mut bytes = 0_usize;
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| {
                    Error::Network("Diagnostic archive response stream failed".to_string())
                })?;
                bytes = bytes.checked_add(chunk.len()).ok_or_else(|| {
                    Error::RequestRejected(
                        "Diagnostic archive response byte accounting overflow".to_string(),
                    )
                })?;
                if bytes > request.max_bytes || bytes > MAX_INSPECT_ARCHIVE_BYTES {
                    return Err(Error::RequestRejected(
                        "Diagnostic archive response exceeds the advertised byte limit".to_string(),
                    ));
                }
                tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                    .await
                    .map_err(|_| {
                        Error::InvalidPath(
                            "Failed to write encrypted archive staging file".to_string(),
                        )
                    })?;
            }
            tokio::io::AsyncWriteExt::flush(&mut file)
                .await
                .map_err(|_| {
                    Error::InvalidPath("Failed to sync encrypted archive staging file".to_string())
                })?;
            file.into_std().await.sync_all().map_err(|_| {
                Error::InvalidPath("Failed to sync encrypted archive staging file".to_string())
            })?;
            Ok(EncryptedInspectArchive {
                file: temporary,
                bytes: u64::try_from(bytes).map_err(|_| {
                    Error::RequestRejected(
                        "Diagnostic archive response byte accounting overflow".to_string(),
                    )
                })?,
            })
        };
        tokio::time::timeout(request.timeout, operation)
            .await
            .map_err(|_| Error::Network("Diagnostic archive request timed out".to_string()))?
    }
}

#[async_trait]
impl ConfigApi for AdminClient {
    async fn get_config(&self, selector: &str) -> Result<ConfigDocument> {
        let (_, body) = self
            .config_request(
                Method::GET,
                "/get-config-kv",
                Some(&[("key", selector)]),
                None,
            )
            .await?;
        let content = std::str::from_utf8(body.as_slice())
            .map_err(|_| Error::General("Server returned invalid configuration text".to_string()))?
            .to_string();
        Ok(ConfigDocument { content })
    }

    async fn get_full_config(&self) -> Result<ConfigDocument> {
        let (_, body) = self
            .config_request(Method::GET, "/config", None, None)
            .await?;
        let content = std::str::from_utf8(body.as_slice())
            .map_err(|_| Error::General("Server returned invalid configuration text".to_string()))?
            .to_string();
        Ok(ConfigDocument { content })
    }

    async fn set_config(&self, directive: &str) -> Result<ConfigMutationResult> {
        let (headers, _) = self
            .config_request(
                Method::PUT,
                "/set-config-kv",
                None,
                Some(Zeroizing::new(directive.as_bytes().to_vec())),
            )
            .await?;
        Ok(ConfigMutationResult {
            applied_dynamically: config_applied(&headers),
        })
    }

    async fn delete_config(&self, directive: &str) -> Result<ConfigMutationResult> {
        let (headers, _) = self
            .config_request(
                Method::DELETE,
                "/del-config-kv",
                None,
                Some(Zeroizing::new(directive.as_bytes().to_vec())),
            )
            .await?;
        Ok(ConfigMutationResult {
            applied_dynamically: config_applied(&headers),
        })
    }

    async fn config_help(
        &self,
        subsystem: Option<&str>,
        key: Option<&str>,
        env_only: bool,
    ) -> Result<ConfigHelp> {
        let mut query = Vec::new();
        if let Some(subsystem) = subsystem {
            query.push(("subSys", subsystem));
        }
        if let Some(key) = key {
            query.push(("key", key));
        }
        if env_only {
            query.push(("env", "true"));
        }
        let (_, body) = self
            .config_request(Method::GET, "/help-config-kv", Some(&query), None)
            .await?;
        serde_json::from_slice(body.as_slice())
            .map_err(|_| Error::General("Server returned invalid configuration help".to_string()))
    }

    async fn config_history(&self, count: usize) -> Result<Vec<ConfigHistoryEntry>> {
        let count = count.to_string();
        let (_, body) = self
            .config_request(
                Method::GET,
                "/list-config-history-kv",
                Some(&[("count", count.as_str())]),
                None,
            )
            .await?;
        serde_json::from_slice(body.as_slice()).map_err(|_| {
            Error::General("Server returned invalid configuration history".to_string())
        })
    }

    async fn restore_config(&self, restore_id: &str) -> Result<()> {
        self.config_request(
            Method::PUT,
            "/restore-config-history-kv",
            Some(&[("restoreId", restore_id)]),
            None,
        )
        .await
        .map(|_| ())
    }

    async fn import_config(&self, document: &ConfigDocument) -> Result<()> {
        self.config_request(
            Method::PUT,
            "/config",
            None,
            Some(Zeroizing::new(document.content.as_bytes().to_vec())),
        )
        .await
        .map(|_| ())
    }

    async fn get_module_switches(&self) -> Result<ModuleSwitches> {
        let (_, body) = self
            .config_request(Method::GET, "/module-switches", None, None)
            .await
            .map_err(|error| match error {
                Error::NotFound(_) => Error::UnsupportedFeature(
                    "Server does not advertise module switch operations".to_string(),
                ),
                other => other,
            })?;
        serde_json::from_slice(body.as_slice())
            .map_err(|_| Error::General("Server returned invalid module switch state".to_string()))
    }

    async fn set_module_switches(&self, switches: &ModuleSwitches) -> Result<ModuleSwitches> {
        let body = Zeroizing::new(
            serde_json::to_vec(&serde_json::json!({
                "notify_enabled": switches.notify_enabled,
                "audit_enabled": switches.audit_enabled,
            }))
            .map_err(|_| Error::General("Failed to encode module switch request".to_string()))?,
        );
        let (_, response) = self
            .config_request(Method::PUT, "/module-switches", None, Some(body))
            .await?;
        serde_json::from_slice(response.as_slice())
            .map_err(|_| Error::General("Server returned invalid module switch state".to_string()))
    }
}

fn config_applied(headers: &HeaderMap) -> bool {
    headers
        .get("x-rustfs-config-applied")
        .or_else(|| headers.get("x-minio-config-applied"))
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

#[async_trait]
impl ObservabilityApi for AdminClient {
    async fn scanner_status(&self) -> Result<ScannerStatus> {
        self.request(Method::GET, "/scanner/status", None, None)
            .await
            .map_err(|error| observability_route_error(error, "Scanner status"))
    }

    async fn storage_info(&self) -> Result<StorageInfo> {
        let response: StorageInfoResponse = self
            .request(Method::GET, "/storageinfo", None, None)
            .await
            .map_err(|error| observability_route_error(error, "Storage information"))?;
        Ok(response.info)
    }

    async fn realtime_metrics(&self, query: &MetricsQuery) -> Result<MetricsBatch> {
        if query.samples == 0 || query.samples > MAX_METRICS_SAMPLES {
            return Err(Error::InvalidPath(format!(
                "Metrics samples must be between 1 and {MAX_METRICS_SAMPLES}"
            )));
        }

        let disks = query.disks.join(",");
        let hosts = query.hosts.join(",");
        let interval = query.interval.clone().unwrap_or_default();
        let samples = query.samples.to_string();
        let types = query.types_mask().to_string();
        let mut params = Vec::new();
        if !disks.is_empty() {
            params.push(("disks", disks.as_str()));
        }
        if !hosts.is_empty() {
            params.push(("hosts", hosts.as_str()));
        }
        if !interval.is_empty() {
            params.push(("interval", interval.as_str()));
        }
        params.push(("n", samples.as_str()));
        params.push(("types", types.as_str()));
        if query.by_disk {
            params.push(("by-disk", "true"));
        }
        if query.by_host {
            params.push(("by-host", "true"));
        }
        if let Some(job_id) = query.job_id.as_deref() {
            params.push(("by-jobID", job_id));
        }
        if let Some(deployment_id) = query.deployment_id.as_deref() {
            params.push(("by-depID", deployment_id));
        }

        let mut url = self.admin_url("/metrics");
        let query_string = params
            .iter()
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    urlencoding::encode(key),
                    urlencoding::encode(value)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        url.push('?');
        url.push_str(&query_string);

        let headers = self.request_headers(&[])?;
        let signed_headers = self.sign_request(&Method::GET, &url, &headers, &[]).await?;
        let mut request_builder = self.http_client.get(&url);
        for (name, value) in &signed_headers {
            request_builder = request_builder.header(name, value);
        }
        let response = request_builder
            .send()
            .await
            .map_err(|error| Error::Network(format!("Request failed: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(observability_route_error(
                self.map_error(status, &body),
                "Realtime metrics",
            ));
        }

        let mut stream = response.bytes_stream();
        let mut pending = Vec::new();
        let mut snapshots = Vec::new();
        let mut encoded_bytes = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|error| Error::Network(format!("Failed to read response: {error}")))?;
            encoded_bytes = encoded_bytes.saturating_add(chunk.len());
            if encoded_bytes > MAX_METRICS_RESPONSE_BYTES {
                return Err(Error::General(format!(
                    "Metrics response exceeded the {MAX_METRICS_RESPONSE_BYTES}-byte limit"
                )));
            }
            pending.extend_from_slice(&chunk);
            parse_metrics_records(&mut pending, &mut snapshots, false)?;
            if snapshots.len() > usize::from(query.samples) {
                return Err(Error::General(format!(
                    "Metrics response exceeded the requested {} record limit",
                    query.samples
                )));
            }
        }
        parse_metrics_records(&mut pending, &mut snapshots, true)?;
        if snapshots.len() > usize::from(query.samples) {
            return Err(Error::General(format!(
                "Metrics response exceeded the requested {} record limit",
                query.samples
            )));
        }

        Ok(MetricsBatch {
            snapshots,
            encoded_bytes,
        })
    }
}

fn observability_route_error(error: Error, feature: &str) -> Error {
    match error {
        Error::NotFound(_) => {
            Error::UnsupportedFeature(format!("{feature} is unavailable on this RustFS server"))
        }
        error => error,
    }
}

fn safe_config_error(status: StatusCode) -> Error {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            Error::Auth("Server denied configuration access".to_string())
        }
        StatusCode::NOT_FOUND => Error::NotFound(
            "Configuration API route or requested resource was not found".to_string(),
        ),
        StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => {
            Error::Conflict("Server configuration changed before mutation".to_string())
        }
        StatusCode::NOT_IMPLEMENTED => Error::UnsupportedFeature(
            "Server does not implement this configuration operation".to_string(),
        ),
        StatusCode::BAD_REQUEST => Error::Config(
            "Server rejected the configuration request; review `rc admin config help`".to_string(),
        ),
        _ => Error::Network(format!(
            "Configuration request failed with HTTP {}",
            status.as_u16()
        )),
    }
}

fn parse_metrics_records(
    pending: &mut Vec<u8>,
    snapshots: &mut Vec<RealtimeMetrics>,
    flush: bool,
) -> Result<()> {
    loop {
        let record_end = pending.iter().position(|byte| *byte == b'\n');
        let record = match record_end {
            Some(index) => pending.drain(..=index).collect::<Vec<_>>(),
            None if flush && !pending.is_empty() => std::mem::take(pending),
            None => break,
        };
        let record = record.strip_suffix(b"\n").unwrap_or(record.as_slice());
        let record = record.strip_suffix(b"\r").unwrap_or(record);
        if record.is_empty() {
            continue;
        }
        if record.len() > MAX_METRICS_LINE_BYTES {
            return Err(Error::General(format!(
                "Metrics record exceeded the {MAX_METRICS_LINE_BYTES}-byte record limit"
            )));
        }
        snapshots.push(serde_json::from_slice(record).map_err(Error::Json)?);
    }
    if pending.len() > MAX_METRICS_LINE_BYTES {
        return Err(Error::General(format!(
            "Metrics record exceeded the {MAX_METRICS_LINE_BYTES}-byte record limit"
        )));
    }
    Ok(())
}

#[async_trait]
impl AdminApi for AdminClient {
    // ==================== Cluster Operations ====================

    async fn cluster_info(&self) -> Result<ClusterInfo> {
        let response: ServerInfoResponse = self.request(Method::GET, "/info", None, None).await?;
        Ok(response.info)
    }

    async fn heal_status(&self) -> Result<HealStatus> {
        let response: BackgroundHealStatusResponse = self
            .request(Method::POST, "/background-heal/status", None, None)
            .await?;
        Ok(response.into())
    }

    async fn heal_start(&self, request: HealStartRequest) -> Result<HealStatus> {
        let path = rustfs_heal_path(&request)?;
        let body = rustfs_heal_start_body(&request)?;
        let response: HealStartSuccessResponse =
            self.request(Method::POST, &path, None, Some(&body)).await?;
        Ok(response.into_status(&request))
    }

    async fn heal_task_status(&self, request: HealTaskRequest) -> Result<HealStatus> {
        let path = rustfs_heal_task_path(&request)?;
        let query = [("clientToken", request.client_token.as_str())];
        let response: HealTaskStatusResponse = self
            .request(Method::POST, &path, Some(&query), None)
            .await?;
        Ok(response.into_status(&request))
    }

    async fn heal_stop(&self) -> Result<()> {
        let body = rustfs_heal_body(&HealStartRequest::default())?;
        self.request_no_response(
            Method::POST,
            "/heal/",
            Some(&[("forceStop", "true")]),
            Some(&body),
        )
        .await
    }

    async fn heal_task_stop(&self, request: HealTaskRequest) -> Result<HealStatus> {
        let path = rustfs_heal_task_path(&request)?;
        let query = [
            ("clientToken", request.client_token.as_str()),
            ("forceStop", "true"),
        ];
        let response: HealTaskStatusResponse = self
            .request(Method::POST, &path, Some(&query), None)
            .await?;
        Ok(response.into_status(&request))
    }

    async fn list_pools(&self) -> Result<Vec<PoolStatus>> {
        self.request(Method::GET, "/pools/list", None, None).await
    }

    async fn pool_status(&self, target: PoolTarget) -> Result<PoolStatus> {
        let query = pool_target_query(&target);
        let response: PoolStatusResponse = self
            .request(Method::GET, "/pools/status", Some(&query), None)
            .await?;
        Ok(response.pool)
    }

    async fn decommission_start(&self, target: PoolTarget) -> Result<()> {
        let query = pool_target_query(&target);
        self.request_no_response(Method::POST, "/pools/decommission", Some(&query), None)
            .await
    }

    async fn decommission_cancel(&self, target: PoolTarget) -> Result<()> {
        let query = pool_target_query(&target);
        self.request_no_response(Method::POST, "/pools/cancel", Some(&query), None)
            .await
    }

    async fn decommission_clear(&self, target: PoolTarget) -> Result<()> {
        let query = pool_target_query(&target);
        self.request_no_response(Method::POST, "/pools/clear", Some(&query), None)
            .await
    }

    async fn decommission_status(&self, target: Option<PoolTarget>) -> Result<DecommissionStatus> {
        if let Some(target) = target {
            let query = pool_target_query(&target);
            let pool = self
                .request::<DecommissionPoolStatus>(
                    Method::GET,
                    "/decommission/status",
                    Some(&query),
                    None,
                )
                .await?;
            Ok(DecommissionStatus { pools: vec![pool] })
        } else {
            self.request(Method::GET, "/decommission/status", None, None)
                .await
        }
    }

    async fn rebalance_start(&self) -> Result<RebalanceStartResult> {
        self.request(Method::POST, "/rebalance/start", None, None)
            .await
    }

    async fn rebalance_status(&self) -> Result<RebalanceStatus> {
        self.request(Method::GET, "/rebalance/status", None, None)
            .await
    }

    async fn rebalance_stop(&self) -> Result<()> {
        self.request_no_response(Method::POST, "/rebalance/stop", None, None)
            .await
    }

    // ==================== User Operations ====================

    async fn list_users(&self) -> Result<Vec<User>> {
        let response: UserListResponse =
            self.request(Method::GET, "/list-users", None, None).await?;

        Ok(response
            .0
            .into_iter()
            .map(|(access_key, info)| User {
                access_key,
                secret_key: None,
                status: if info.status == "disabled" {
                    UserStatus::Disabled
                } else {
                    UserStatus::Enabled
                },
                policy_name: info.policy_name,
                member_of: info.member_of.unwrap_or_default(),
            })
            .collect())
    }

    async fn get_user(&self, access_key: &str) -> Result<User> {
        let query = [("accessKey", access_key)];
        let response: UserInfo = self
            .request(Method::GET, "/user-info", Some(&query), None)
            .await?;

        Ok(User {
            access_key: access_key.to_string(),
            secret_key: None,
            status: if response.status == "disabled" {
                UserStatus::Disabled
            } else {
                UserStatus::Enabled
            },
            policy_name: response.policy_name,
            member_of: response.member_of.unwrap_or_default(),
        })
    }

    async fn create_user(&self, access_key: &str, secret_key: &str) -> Result<User> {
        let query = [("accessKey", access_key)];
        let body = serde_json::to_vec(&CreateUserRequest {
            secret_key: secret_key.to_string(),
            status: "enabled".to_string(),
        })
        .map_err(Error::Json)?;

        self.request_no_response(Method::PUT, "/add-user", Some(&query), Some(&body))
            .await?;

        Ok(User {
            access_key: access_key.to_string(),
            secret_key: Some(secret_key.to_string()),
            status: UserStatus::Enabled,
            policy_name: None,
            member_of: vec![],
        })
    }

    async fn delete_user(&self, access_key: &str) -> Result<()> {
        let query = [("accessKey", access_key)];
        self.request_no_response(Method::DELETE, "/remove-user", Some(&query), None)
            .await
    }

    async fn set_user_status(&self, access_key: &str, status: UserStatus) -> Result<()> {
        let status_str = match status {
            UserStatus::Enabled => "enabled",
            UserStatus::Disabled => "disabled",
        };
        let query = [("accessKey", access_key), ("status", status_str)];
        self.request_no_response(Method::PUT, "/set-user-status", Some(&query), None)
            .await
    }

    // ==================== Policy Operations ====================

    async fn list_policies(&self) -> Result<Vec<PolicyInfo>> {
        let response: PolicyListResponse = self
            .request(Method::GET, "/list-canned-policies", None, None)
            .await?;

        Ok(response
            .0
            .into_keys()
            .map(|name| PolicyInfo { name })
            .collect())
    }

    async fn get_policy(&self, name: &str) -> Result<Policy> {
        let query = [("name", name)];
        let policy_doc: serde_json::Value = self
            .request(Method::GET, "/info-canned-policy", Some(&query), None)
            .await?;

        Ok(Policy {
            name: name.to_string(),
            policy: serde_json::to_string_pretty(&policy_doc).unwrap_or_default(),
        })
    }

    async fn create_policy(&self, name: &str, policy_document: &str) -> Result<()> {
        let query = [("name", name)];
        let body = policy_document.as_bytes();
        self.request_no_response(Method::PUT, "/add-canned-policy", Some(&query), Some(body))
            .await
    }

    async fn delete_policy(&self, name: &str) -> Result<()> {
        let query = [("name", name)];
        self.request_no_response(Method::DELETE, "/remove-canned-policy", Some(&query), None)
            .await
    }

    async fn attach_policy(
        &self,
        policy_names: &[String],
        entity_type: PolicyEntity,
        entity_name: &str,
    ) -> Result<()> {
        let policy_name = policy_names.join(",");
        let is_group = entity_type == PolicyEntity::Group;

        let query = [
            ("policyName", policy_name.as_str()),
            ("userOrGroup", entity_name),
            ("isGroup", if is_group { "true" } else { "false" }),
        ];

        self.request_no_response(Method::PUT, "/set-user-or-group-policy", Some(&query), None)
            .await
    }

    async fn detach_policy(
        &self,
        policy_names: &[String],
        entity_type: PolicyEntity,
        entity_name: &str,
    ) -> Result<()> {
        let entity = match entity_type {
            PolicyEntity::User => PolicyDetachEntity::User,
            PolicyEntity::Group => PolicyDetachEntity::Group,
        };
        let request =
            PolicyDetachRequest::new(policy_names.to_vec(), entity, entity_name.to_string())?;
        IamMutationApi::detach_policies(self, &request)
            .await
            .map(|_| ())
    }

    // ==================== Group Operations ====================

    async fn list_groups(&self) -> Result<Vec<String>> {
        let response: Vec<String> = self.request(Method::GET, "/groups", None, None).await?;
        Ok(response)
    }

    async fn get_group(&self, name: &str) -> Result<Group> {
        let query = [("group", name)];
        let response: Group = self
            .request(Method::GET, "/group", Some(&query), None)
            .await?;
        Ok(response)
    }

    async fn create_group(&self, name: &str, members: Option<&[String]>) -> Result<Group> {
        let body = serde_json::to_vec(&CreateGroupRequest {
            group: name.to_string(),
            members: members.map(|m| m.to_vec()),
        })
        .map_err(Error::Json)?;

        self.request_no_response(Method::POST, "/groups", None, Some(&body))
            .await?;

        Ok(Group {
            name: name.to_string(),
            policy: None,
            members: members.map(|m| m.to_vec()).unwrap_or_default(),
            status: GroupStatus::Enabled,
        })
    }

    async fn delete_group(&self, name: &str) -> Result<()> {
        let path = format!("/group/{}", urlencoding::encode(name));
        self.request_no_response(Method::DELETE, &path, None, None)
            .await
    }

    async fn set_group_status(&self, name: &str, status: GroupStatus) -> Result<()> {
        let status_str = match status {
            GroupStatus::Enabled => "enabled",
            GroupStatus::Disabled => "disabled",
        };
        let query = [("group", name), ("status", status_str)];
        self.request_no_response(Method::PUT, "/set-group-status", Some(&query), None)
            .await
    }

    async fn add_group_members(&self, group: &str, members: &[String]) -> Result<()> {
        let body = serde_json::to_vec(&UpdateGroupMembersRequest {
            group: group.to_string(),
            members: members.to_vec(),
            is_remove: false,
            status: "enabled".to_string(),
        })
        .map_err(Error::Json)?;

        self.request_no_response(Method::PUT, "/update-group-members", None, Some(&body))
            .await
    }

    async fn remove_group_members(&self, group: &str, members: &[String]) -> Result<()> {
        let body = serde_json::to_vec(&UpdateGroupMembersRequest {
            group: group.to_string(),
            members: members.to_vec(),
            is_remove: true,
            status: "enabled".to_string(),
        })
        .map_err(Error::Json)?;

        self.request_no_response(Method::PUT, "/update-group-members", None, Some(&body))
            .await
    }

    // ==================== Service Account Operations ====================

    async fn list_service_accounts(&self, user: Option<&str>) -> Result<Vec<ServiceAccount>> {
        let query: Vec<(&str, &str)> = user.map(|u| vec![("user", u)]).unwrap_or_default();
        let query_ref: Option<&[(&str, &str)]> = if query.is_empty() { None } else { Some(&query) };

        let response: ServiceAccountListResponse = self
            .request(Method::GET, "/list-service-accounts", query_ref, None)
            .await?;

        Ok(response
            .accounts
            .unwrap_or_default()
            .into_iter()
            .map(|sa| ServiceAccount {
                access_key: sa.access_key,
                secret_key: None,
                parent_user: sa.parent_user,
                policy: None,
                account_status: sa.account_status,
                expiration: sa.expiration,
                name: sa.name,
                description: sa.description,
                implied_policy: sa.implied_policy,
            })
            .collect())
    }

    async fn get_service_account(&self, access_key: &str) -> Result<ServiceAccount> {
        let query = [("accessKey", access_key)];
        let response: ServiceAccount = self
            .request(Method::GET, "/info-service-account", Some(&query), None)
            .await?;

        let mut response = response;
        if response.access_key.is_empty() {
            response.access_key = access_key.to_string();
        }
        Ok(response)
    }

    async fn create_service_account(
        &self,
        request: CreateServiceAccountRequest,
    ) -> Result<ServiceAccount> {
        let body = serde_json::to_vec(&request).map_err(Error::Json)?;
        let response: ServiceAccountCreateResponse = self
            .request(Method::PUT, "/add-service-accounts", None, Some(&body))
            .await?;

        Ok(ServiceAccount {
            access_key: response.credentials.access_key,
            secret_key: Some(response.credentials.secret_key),
            expiration: response.credentials.expiration,
            parent_user: None,
            policy: None,
            account_status: None,
            name: None,
            description: None,
            implied_policy: None,
        })
    }

    async fn update_service_account(
        &self,
        access_key: &str,
        request: UpdateServiceAccountRequest,
    ) -> Result<()> {
        let query = [("accessKey", access_key)];
        let body = serde_json::to_vec(&request).map_err(Error::Json)?;
        self.request_no_response(
            Method::POST,
            "/update-service-account",
            Some(&query),
            Some(&body),
        )
        .await
    }

    async fn delete_service_account(&self, access_key: &str) -> Result<()> {
        let query = [("accessKey", access_key)];
        self.request_no_response(
            Method::DELETE,
            "/delete-service-accounts",
            Some(&query),
            None,
        )
        .await
    }

    async fn get_access_key_info(&self, access_key: &str) -> Result<AccessKeyInfo> {
        let query = [("accessKey", access_key)];
        self.request(Method::GET, "/info-access-key", Some(&query), None)
            .await
    }

    // ==================== Bucket Quota Operations ====================

    async fn set_bucket_quota(&self, bucket: &str, quota: u64) -> Result<BucketQuota> {
        let path = format!("/quota/{}", urlencoding::encode(bucket));
        let body = serde_json::to_vec(&SetBucketQuotaApiRequest {
            quota,
            quota_type: "HARD".to_string(),
        })
        .map_err(Error::Json)?;

        self.request(Method::PUT, &path, None, Some(&body)).await
    }

    async fn get_bucket_quota(&self, bucket: &str) -> Result<BucketQuota> {
        let path = format!("/quota/{}", urlencoding::encode(bucket));
        self.request(Method::GET, &path, None, None).await
    }

    async fn clear_bucket_quota(&self, bucket: &str) -> Result<BucketQuota> {
        let path = format!("/quota/{}", urlencoding::encode(bucket));
        self.request(Method::DELETE, &path, None, None).await
    }

    // ==================== Tier Operations ====================

    async fn list_tiers(&self) -> Result<Vec<rc_core::admin::TierConfig>> {
        self.request(Method::GET, "/tier", None, None).await
    }

    async fn tier_stats(&self) -> Result<serde_json::Value> {
        self.request(Method::GET, "/tier-stats", None, None).await
    }

    async fn add_tier(&self, config: rc_core::admin::TierConfig) -> Result<()> {
        let body = serde_json::to_vec(&config).map_err(Error::Json)?;
        self.request_no_response(Method::PUT, "/tier", None, Some(&body))
            .await
    }

    async fn edit_tier(&self, name: &str, creds: rc_core::admin::TierCreds) -> Result<()> {
        let path = format!("/tier/{}", urlencoding::encode(name));
        let body = serde_json::to_vec(&creds).map_err(Error::Json)?;
        self.request_no_response(Method::POST, &path, None, Some(&body))
            .await
    }

    async fn remove_tier(&self, name: &str, force: bool) -> Result<()> {
        let path = format!("/tier/{}", urlencoding::encode(name));
        if force {
            let query: &[(&str, &str)] = &[("force", "true")];
            self.request_no_response(Method::DELETE, &path, Some(query), None)
                .await
        } else {
            self.request_no_response(Method::DELETE, &path, None, None)
                .await
        }
    }

    async fn run_manual_transition(
        &self,
        request: ManualTransitionRunRequest,
    ) -> Result<ManualTransitionRunResponse> {
        let dry_run = if request.dry_run { "true" } else { "false" };
        let max_objects = request.max_objects.to_string();
        let mut query = vec![
            ("bucket", request.bucket.as_str()),
            ("prefix", request.prefix.as_str()),
            ("dryRun", dry_run),
            ("maxObjects", max_objects.as_str()),
        ];
        let max_duration_seconds = request
            .max_duration_seconds
            .map(|duration| duration.to_string());
        if let Some(max_duration_seconds) = max_duration_seconds.as_deref() {
            query.push(("maxDurationSeconds", max_duration_seconds));
        }
        if let Some(tier) = request.tier.as_deref() {
            query.push(("tier", tier));
        }

        self.request(Method::POST, "/ilm/transition/run", Some(&query), None)
            .await
    }

    // ==================== Replication Target Operations ====================

    async fn set_remote_target(
        &self,
        bucket: &str,
        target: rc_core::replication::BucketTarget,
        update: bool,
    ) -> Result<String> {
        let body = serde_json::to_vec(&target).map_err(Error::Json)?;
        if update {
            let query: &[(&str, &str)] = &[("bucket", bucket), ("update", "true")];
            self.request(Method::PUT, "/set-remote-target", Some(query), Some(&body))
                .await
        } else {
            let query: &[(&str, &str)] = &[("bucket", bucket)];
            self.request(Method::PUT, "/set-remote-target", Some(query), Some(&body))
                .await
        }
    }

    async fn list_remote_targets(
        &self,
        bucket: &str,
    ) -> Result<Vec<rc_core::replication::BucketTarget>> {
        let query: &[(&str, &str)] = &[("bucket", bucket)];
        self.request(Method::GET, "/list-remote-targets", Some(query), None)
            .await
    }

    async fn remove_remote_target(&self, bucket: &str, arn: &str) -> Result<()> {
        let query: &[(&str, &str)] = &[("bucket", bucket), ("arn", arn)];
        self.request_no_response(Method::DELETE, "/remove-remote-target", Some(query), None)
            .await
    }

    async fn service_action(&self, action: &str) -> Result<ServiceActionResult> {
        let query: &[(&str, &str)] = &[("action", action)];
        self.request(Method::POST, "/service", Some(query), None)
            .await
    }

    async fn site_replication_info(&self) -> Result<SiteReplicationInfo> {
        self.request_site_replication(
            Method::GET,
            "/site-replication/info",
            None,
            "Site replication info",
            None,
            None,
        )
        .await
    }

    async fn site_replication_edit(
        &self,
        peer: &SiteReplicationPeer,
    ) -> Result<ReplicateEditStatus> {
        let body = serde_json::to_vec(peer).map_err(Error::Json)?;
        let mut status: ReplicateEditStatus = self
            .request_site_replication(
                Method::PUT,
                "/site-replication/edit",
                Some(&body),
                "Site replication edit",
                Some("Site replication edit"),
                None,
            )
            .await?;
        self.sanitize_site_replication_status(&mut status);
        if status.success {
            return Ok(status);
        }

        let detail = if status.error_detail.trim().is_empty() {
            status.status.clone()
        } else {
            status.error_detail.clone()
        };
        if site_replication_server_invalid_state(None, &detail) {
            Err(Error::Conflict(
                "Site replication edit conflicts with current server state".to_string(),
            ))
        } else {
            Err(Error::General(
                "Site replication edit was rejected by the server".to_string(),
            ))
        }
    }

    async fn site_replication_resync(
        &self,
        operation: SiteReplicationResyncOperation,
        peer: &SiteReplicationPeer,
    ) -> Result<SiteReplicationResyncStatus> {
        let (operation_label, mutation_outcome_label) = match &operation {
            SiteReplicationResyncOperation::Start => (
                "Site replication resync start",
                Some("Site replication resync start"),
            ),
            SiteReplicationResyncOperation::Status => ("Site replication resync status", None),
            SiteReplicationResyncOperation::Cancel => (
                "Site replication resync cancel",
                Some("Site replication resync cancel"),
            ),
        };
        let path = format!(
            "/site-replication/resync/op?operation={}",
            operation.as_str()
        );
        let body = serde_json::to_vec(peer).map_err(Error::Json)?;
        let mut status = self
            .request_site_replication(
                Method::PUT,
                &path,
                Some(&body),
                operation_label,
                mutation_outcome_label,
                mutation_outcome_label,
            )
            .await?;
        if let Err(error) = validate_site_replication_resync_status(&operation, &status) {
            if operation.is_mutation() {
                return Err(site_replication_response_rejected(
                    mutation_outcome_label,
                    "server returned an invalid resync response".to_string(),
                ));
            }
            return Err(error);
        }
        status.capture_semantics();
        self.sanitize_site_replication_resync_status(&mut status);
        Ok(status)
    }

    async fn site_replication_add(&self, sites: &[PeerSiteSpec]) -> Result<serde_json::Value> {
        let body = serde_json::to_vec(sites).map_err(Error::Json)?;
        self.request(Method::PUT, "/site-replication/add", None, Some(&body))
            .await
    }

    async fn site_replication_status(
        &self,
        options: &SiteStatusOptions,
    ) -> Result<serde_json::Value> {
        let mut query: Vec<(&str, &str)> = Vec::new();
        for (flag, enabled) in [
            ("buckets", options.buckets),
            ("users", options.users),
            ("groups", options.groups),
            ("policies", options.policies),
            ("metrics", options.metrics),
            ("peer-state", options.peer_state),
            ("ilm-expiry-rules", options.ilm_expiry_rules),
        ] {
            if enabled {
                query.push((flag, "true"));
            }
        }
        self.request(Method::GET, "/site-replication/status", Some(&query), None)
            .await
    }

    async fn site_replication_remove(&self, spec: &SiteRemoveSpec) -> Result<serde_json::Value> {
        let body = serde_json::to_vec(spec).map_err(Error::Json)?;
        self.request(Method::PUT, "/site-replication/remove", None, Some(&body))
            .await
    }
}

#[async_trait]
impl ReplicationDiffApi for AdminClient {
    async fn replication_diff(
        &self,
        bucket: &str,
        prefix: Option<&str>,
    ) -> Result<ReplicationDiff> {
        let mut query = vec![("bucket", bucket)];
        if let Some(prefix) = prefix {
            query.push(("prefix", prefix));
        }

        self.request_bounded_json(
            Method::POST,
            "/replication/diff",
            Some(&query),
            None,
            BoundedJsonResponse {
                max_bytes: MAX_REPLICATION_DIFF_RESPONSE_BYTES,
                name: "Replication diff response",
                error_mapper: Self::map_replication_diff_error,
            },
        )
        .await
    }
}

#[async_trait]
impl ReplicationInspectionApi for AdminClient {
    async fn replication_metrics(&self, bucket: &str) -> Result<ReplicationMetrics> {
        let query = [("bucket", bucket)];
        let mut metrics: ReplicationMetrics = self
            .request_bounded_json(
                Method::GET,
                "/replicationmetrics",
                Some(&query),
                None,
                BoundedJsonResponse {
                    max_bytes: MAX_REPLICATION_INSPECTION_RESPONSE_BYTES,
                    name: "Replication metrics response",
                    error_mapper: Self::map_replication_inspection_error,
                },
            )
            .await?;
        for target in metrics.stats.values() {
            if target
                .fail_stats
                .as_ref()
                .is_some_and(|authoritative| authoritative != &target.failed)
            {
                return Err(Error::General(
                    "Replication metrics response contains inconsistent failed totals".into(),
                ));
            }
            if [
                target.latency.avg,
                target.latency.curr,
                target.latency.max,
                target.xfer_rate_lrg.avg,
                target.xfer_rate_lrg.curr,
                target.xfer_rate_lrg.peak,
                target.xfer_rate_sml.avg,
                target.xfer_rate_sml.curr,
                target.xfer_rate_sml.peak,
                target.current_bandwidth_bytes_per_sec,
            ]
            .into_iter()
            .any(|value| !value.is_finite() || value < 0.0)
            {
                return Err(Error::General(
                    "Replication metrics response contains invalid negative observations".into(),
                ));
            }
        }
        self.sanitize_replication_metrics(&mut metrics);
        Ok(metrics)
    }

    async fn replication_mrf(&self, bucket: &str) -> Result<ReplicationMrf> {
        let query = [("bucket", bucket)];
        let mut mrf: ReplicationMrf = self
            .request_bounded_json(
                Method::GET,
                "/replication/mrf",
                Some(&query),
                None,
                BoundedJsonResponse {
                    max_bytes: MAX_REPLICATION_INSPECTION_RESPONSE_BYTES,
                    name: "Replication MRF response",
                    error_mapper: Self::map_replication_inspection_error,
                },
            )
            .await?;
        if mrf.bucket != bucket {
            return Err(Error::General(
                "Replication MRF response bucket does not match the request".into(),
            ));
        }
        let target_count = mrf.targets.iter().try_fold(0_u64, |total, target| {
            total.checked_add(target.failed_count)
        });
        let target_size = mrf
            .targets
            .iter()
            .try_fold(0_u64, |total, target| total.checked_add(target.failed_size));
        if target_count != Some(mrf.total_failed_count)
            || target_size != Some(mrf.total_failed_size)
        {
            return Err(Error::General(
                "Replication MRF response contains inconsistent target totals".into(),
            ));
        }
        self.sanitize_replication_mrf(&mut mrf);
        Ok(mrf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::admin::{ClientDevnullRequest, DiagnosticApi};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    #[derive(Debug)]
    struct CapturedAdminRequest {
        method: String,
        target: String,
        headers: String,
        body: Vec<u8>,
    }

    fn read_admin_request(stream: &mut TcpStream) -> CapturedAdminRequest {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut chunk).expect("read HTTP request");
            assert!(read > 0, "client closed connection before headers");
            buffer.extend_from_slice(&chunk[..read]);

            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };

        let headers = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("valid content length"))
            })
            .unwrap_or(0);

        while buffer.len() - header_end < content_length {
            let read = stream.read(&mut chunk).expect("read HTTP request body");
            assert!(read > 0, "client closed connection before body");
            buffer.extend_from_slice(&chunk[..read]);
        }

        let request_line = headers.lines().next().expect("request line");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().expect("request method").to_string();
        let target = parts.next().expect("request target").to_string();
        let body = buffer[header_end..header_end + content_length].to_vec();

        CapturedAdminRequest {
            method,
            target,
            headers,
            body,
        }
    }

    fn read_admin_request_head(stream: &mut TcpStream) -> (String, usize, usize) {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut chunk).expect("read HTTP request headers");
            assert!(read > 0, "client closed connection before headers");
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("valid content length"))
            })
            .unwrap_or(0);
        (headers, content_length, buffer.len() - header_end)
    }

    fn start_admin_test_server(
        response_status: &str,
        response_body: &'static str,
    ) -> (
        String,
        mpsc::Receiver<CapturedAdminRequest>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let (sender, receiver) = mpsc::channel();
        let response_status = response_status.to_string();

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let request = read_admin_request(&mut stream);
            sender.send(request).expect("send captured request");

            let response = format!(
                "HTTP/1.1 {response_status}\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write HTTP response");
        });

        (endpoint, receiver, handle)
    }

    fn start_admin_sequence_server(
        responses: Vec<(&'static str, &'static str)>,
    ) -> (
        String,
        mpsc::Receiver<CapturedAdminRequest>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let (sender, receiver) = mpsc::channel();

        let handle = thread::spawn(move || {
            for (response_status, response_body) in responses {
                let (mut stream, _) = listener.accept().expect("accept request");
                let request = read_admin_request(&mut stream);
                sender.send(request).expect("send captured request");
                let response = format!(
                    "HTTP/1.1 {response_status}\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write HTTP response");
            }
        });

        (endpoint, receiver, handle)
    }

    fn start_admin_raw_response_server(response: Vec<u8>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let _request = read_admin_request(&mut stream);
            let _ = stream.write_all(&response);
        });

        (endpoint, handle)
    }

    fn start_admin_captured_raw_response_server(
        response: Vec<u8>,
    ) -> (
        String,
        mpsc::Receiver<CapturedAdminRequest>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            sender
                .send(read_admin_request(&mut stream))
                .expect("send captured request");
            let _ = stream.write_all(&response);
        });

        (endpoint, receiver, handle)
    }

    fn start_admin_owned_test_server(
        response_status: &str,
        content_type: &str,
        response_body: String,
    ) -> (
        String,
        mpsc::Receiver<CapturedAdminRequest>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let (sender, receiver) = mpsc::channel();
        let response_status = response_status.to_string();
        let content_type = content_type.to_string();

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let request = read_admin_request(&mut stream);
            sender.send(request).expect("send captured request");

            let response = format!(
                "HTTP/1.1 {response_status}\r\ncontent-length: {}\r\ncontent-type: {content_type}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write HTTP response");
        });

        (endpoint, receiver, handle)
    }

    fn start_admin_disconnect_server() -> (
        String,
        mpsc::Receiver<CapturedAdminRequest>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            sender
                .send(read_admin_request(&mut stream))
                .expect("send captured request");
        });

        (endpoint, receiver, handle)
    }

    fn start_admin_declared_length_server(
        response_status: &'static str,
        content_length: usize,
    ) -> (
        String,
        mpsc::Receiver<CapturedAdminRequest>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            sender
                .send(read_admin_request(&mut stream))
                .expect("send captured request");
            let response = format!(
                "HTTP/1.1 {response_status}\r\ncontent-length: {content_length}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n"
            );
            stream
                .write_all(response.as_bytes())
                .expect("write HTTP headers");
        });

        (endpoint, receiver, handle)
    }

    fn start_admin_chunked_overflow_server() -> (
        String,
        mpsc::Receiver<CapturedAdminRequest>,
        mpsc::Receiver<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let (sender, receiver) = mpsc::channel();
        let (completion_sender, completion_receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            sender
                .send(read_admin_request(&mut stream))
                .expect("send captured request");
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .expect("set response write timeout");

            let header = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n";
            let chunk = vec![b'x'; 64 * 1024];
            let mut remaining = MAX_REPLICATION_DIFF_RESPONSE_BYTES;
            let mut write_failed = stream.write_all(header).is_err();
            while remaining > 0 && !write_failed {
                let chunk_len = remaining.min(chunk.len());
                let chunk_header = format!("{chunk_len:x}\r\n");
                write_failed = stream.write_all(chunk_header.as_bytes()).is_err()
                    || stream.write_all(&chunk[..chunk_len]).is_err()
                    || stream.write_all(b"\r\n").is_err();
                remaining -= chunk_len;
            }
            if !write_failed {
                // One additional byte is sufficient to exercise the streaming limit. Sending
                // megabytes beyond the limit can block when the client intentionally stops
                // reading, especially with Windows socket buffering behavior.
                let _ = stream.write_all(b"1\r\nx\r\n");
            }
            let _ = completion_sender.send(());
        });

        (endpoint, receiver, completion_receiver)
    }

    fn start_admin_content_length_test_server(
        response_status: &str,
        content_length: usize,
    ) -> (
        String,
        mpsc::Receiver<CapturedAdminRequest>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let (sender, receiver) = mpsc::channel();
        let response_status = response_status.to_string();

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let request = read_admin_request(&mut stream);
            sender.send(request).expect("send captured request");
            let response = format!(
                "HTTP/1.1 {response_status}\r\ncontent-length: {content_length}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n"
            );
            stream
                .write_all(response.as_bytes())
                .expect("write HTTP response");
        });

        (endpoint, receiver, handle)
    }

    fn start_client_devnull_raw_response_server(
        raw_response: Vec<u8>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let handle = thread::spawn(move || {
            for response_body in [
                CAPABILITY_INFO_RESPONSE,
                RUNTIME_CAPABILITIES_RESPONSE,
                EXTENSIONS_RESPONSE,
                CLUSTER_SNAPSHOT_RESPONSE,
            ] {
                let (mut stream, _) = listener.accept().expect("accept discovery request");
                let _request = read_admin_request(&mut stream);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write discovery response");
            }

            let (mut stream, _) = listener.accept().expect("accept active request");
            let _request = read_admin_request(&mut stream);
            let _write_result = stream.write_all(&raw_response);
        });
        (endpoint, handle)
    }

    fn admin_client_for_endpoint(endpoint: &str) -> AdminClient {
        let alias = Alias::new("test", endpoint, "access", "secret");
        AdminClient::new(&alias).expect("admin client should build")
    }

    fn anonymous_admin_client_for_endpoint(endpoint: &str) -> AdminClient {
        let mut alias = Alias::new("test", endpoint, "", "");
        alias.anonymous = true;
        AdminClient::new(&alias).expect("anonymous admin client should build")
    }

    fn assert_heal_options_body(
        body: &[u8],
        recursive: bool,
        scan_mode: u8,
        remove: bool,
        recreate: bool,
        dry_run: bool,
    ) {
        let value: serde_json::Value =
            serde_json::from_slice(body).expect("heal request body should be JSON");

        assert_eq!(value["recursive"], recursive);
        assert_eq!(value["dryRun"], dry_run);
        assert_eq!(value["remove"], remove);
        assert_eq!(value["recreate"], recreate);
        assert_eq!(value["scanMode"], scan_mode);
        assert_eq!(value["updateParity"], false);
        assert_eq!(value["nolock"], false);
        assert!(value.get("bucket").is_none());
        assert!(value.get("prefix").is_none());
    }

    #[test]
    fn test_admin_url_construction() {
        let alias = Alias::new("test", "http://localhost:9000", "access", "secret");
        let client = AdminClient::new(&alias).unwrap();

        assert_eq!(
            client.admin_url("/list-users"),
            "http://localhost:9000/rustfs/admin/v3/list-users"
        );
    }

    const CAPABILITY_INFO_RESPONSE: &str = r#"{"info":{"mode":"distributed","servers":[{"endpoint":"http://node1:9000","state":"online","version":"1.0.0-beta.10","drives":[]}]}}"#;
    const UNKNOWN_CAPABILITY_INFO_RESPONSE: &str = r#"{"info":{"mode":"distributed","servers":[{"endpoint":"http://node1:9000","state":"online","version":"1.0.0-beta.11","drives":[]}]}}"#;
    const RUNTIME_CAPABILITIES_RESPONSE: &str = r#"{"summary":{"observability":{"state":"supported"},"userspace_profiling":{"state":"disabled","reason":"disabled by configuration"},"memory_sampling":{"state":"unsupported","reason":"not available on this platform"},"platform":{"state":"supported"},"topology":{"state":"unknown","reason":"storage is initializing"},"cluster_snapshot":{"state":"supported"}},"cluster_snapshot_path":"/rustfs/admin/v4/cluster/snapshot","cluster_snapshot_summary":{"state":"supported"},"observability":{},"workload_admission":{},"topology":null,"topology_status":{"state":"unknown"}}"#;
    const EXTENSIONS_RESPONSE: &str = r#"{"extensions":[{"schema_version":"rustfs.extension-schema.v1","extension_id":"ops.diagnostics","display_name":"Operations Diagnostics","provider":"rustfs","version":"1","kind":"ops_diagnostics","runtime":{"api_version":"v1","boundary":"builtin"},"capabilities":[],"disabled_by_default":false}],"runtime_capabilities":{},"cluster_snapshot":{},"external_plugin_flow":{}}"#;
    const CLUSTER_SNAPSHOT_RESPONSE: &str = r#"{"snapshot":{"summary":{"runtime":{"state":"supported"},"topology":{"state":"supported"},"membership":{"state":"supported"},"peer_health":{"state":"supported"},"rpc_boundary":{"state":"supported"},"observability":{"state":"supported"},"workload_admission":{"state":"supported"},"actionable_pressure":{"state":"disabled"}},"runtime_capabilities_path":"/rustfs/admin/v4/runtime/capabilities","extensions_catalog_path":"/rustfs/admin/v4/extensions/catalog"}}"#;
    const CLIENT_DEVNULL_RESPONSE: &str = r#"{"kind":"client-devnull","measured":true,"aggregate_write_throughput_bytes_per_sec":2048.0,"rx_bytes":65536,"duration_secs":0.5}"#;

    #[tokio::test]
    async fn capability_discovery_uses_v4_routes_and_classifies_beta10_stubs() {
        let (endpoint, receiver, handle) = start_admin_sequence_server(vec![
            ("200 OK", CAPABILITY_INFO_RESPONSE),
            ("200 OK", RUNTIME_CAPABILITIES_RESPONSE),
            ("200 OK", EXTENSIONS_RESPONSE),
            ("200 OK", CLUSTER_SNAPSHOT_RESPONSE),
        ]);
        let client = admin_client_for_endpoint(&endpoint);

        let report = client
            .discover_capabilities(false)
            .await
            .expect("capability discovery should succeed");
        let second_client = admin_client_for_endpoint(&endpoint);
        let cached = second_client
            .discover_capabilities(false)
            .await
            .expect("process-shared cached discovery should succeed");

        assert_eq!(cached, report);
        assert_eq!(report.server_version.as_deref(), Some("1.0.0-beta.10"));
        assert_eq!(report.extensions.len(), 1);
        assert!(report.cluster.summary.is_some());
        assert!(report.capabilities.iter().any(|capability| {
            capability.name == IAM_POLICY_ENTITIES_CAPABILITY
                && capability.availability == CapabilityAvailability::Available
        }));
        assert!(report.capabilities.iter().any(|capability| {
            capability.name == IAM_POLICY_DETACH_CAPABILITY
                && capability.availability == CapabilityAvailability::Available
        }));
        assert!(report.capabilities.iter().any(|capability| {
            capability.name == "runtime.userspace-profiling"
                && capability.availability == CapabilityAvailability::Disabled
        }));
        assert!(report.capabilities.iter().any(|capability| {
            capability.name == "runtime.memory-sampling"
                && capability.availability == CapabilityAvailability::Unsupported
                && capability.reason.as_deref() == Some("not available on this platform")
        }));
        assert!(report.capabilities.iter().any(|capability| {
            capability.name == "admin.data-usage"
                && capability.availability == CapabilityAvailability::Available
        }));
        assert!(report.capabilities.iter().any(|capability| {
            capability.name == "listen_notification"
                && capability.availability == CapabilityAvailability::Available
                && capability.reason.is_none()
        }));
        for name in [
            "admin.batch",
            "admin.ldap-mutation",
            "admin.logs",
            "admin.trace",
            "admin.update",
        ] {
            assert!(report.capabilities.iter().any(|capability| {
                capability.name == name
                    && capability.availability == CapabilityAvailability::Stubbed
            }));
        }
        for name in [
            "admin.diagnostics.health-snapshot",
            "admin.diagnostics.cluster-snapshot",
            "admin.diagnostics.extensions-catalog",
            "admin.diagnostics.drive-observations",
            "admin.diagnostics.client-devnull",
        ] {
            assert!(report.capabilities.iter().any(|capability| {
                capability.name == name
                    && capability.availability == CapabilityAvailability::Available
            }));
        }
        assert!(report.capabilities.iter().any(|capability| {
            capability.name == "admin.diagnostics.inspect-archive"
                && capability.availability == CapabilityAvailability::Unsupported
        }));
        for name in [
            "admin.diagnostics.object-speedtest",
            "admin.diagnostics.network-speedtest",
            "admin.diagnostics.site-speedtest",
            "admin.site-replication.netperf",
        ] {
            assert!(report.capabilities.iter().any(|capability| {
                capability.name == name
                    && capability.availability == CapabilityAvailability::Stubbed
            }));
        }

        let targets = (0..4)
            .map(|_| receiver.recv().expect("captured request").target)
            .collect::<Vec<_>>();
        assert_eq!(
            targets,
            vec![
                "/rustfs/admin/v3/info",
                "/rustfs/admin/v4/runtime/capabilities",
                "/rustfs/admin/v4/extensions/catalog",
                "/rustfs/admin/v4/cluster/snapshot",
            ]
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn capability_discovery_fails_closed_for_unknown_server_versions() {
        let (endpoint, receiver, handle) = start_admin_sequence_server(vec![
            ("200 OK", UNKNOWN_CAPABILITY_INFO_RESPONSE),
            ("200 OK", RUNTIME_CAPABILITIES_RESPONSE),
            ("200 OK", EXTENSIONS_RESPONSE),
            ("200 OK", CLUSTER_SNAPSHOT_RESPONSE),
        ]);
        let client = admin_client_for_endpoint(&endpoint);

        let report = client
            .discover_capabilities(false)
            .await
            .expect("capability discovery should succeed");

        for name in [
            "admin.diagnostics.health-snapshot",
            "admin.diagnostics.drive-observations",
            "admin.diagnostics.client-devnull",
            "admin.diagnostics.inspect-archive",
            "admin.diagnostics.object-speedtest",
            "admin.diagnostics.network-speedtest",
            "admin.diagnostics.site-speedtest",
            "admin.site-replication.netperf",
        ] {
            assert!(report.capabilities.iter().any(|capability| {
                capability.name == name
                    && capability.availability == CapabilityAvailability::Unknown
            }));
        }
        for name in [
            "admin.diagnostics.cluster-snapshot",
            "admin.diagnostics.extensions-catalog",
        ] {
            assert!(report.capabilities.iter().any(|capability| {
                capability.name == name
                    && capability.availability == CapabilityAvailability::Available
            }));
        }

        let targets = (0..4)
            .map(|_| receiver.recv().expect("captured request").target)
            .collect::<Vec<_>>();
        assert_eq!(
            targets,
            vec![
                "/rustfs/admin/v3/info",
                "/rustfs/admin/v4/runtime/capabilities",
                "/rustfs/admin/v4/extensions/catalog",
                "/rustfs/admin/v4/cluster/snapshot",
            ]
        );
        handle.join().expect("server thread should finish");
    }

    #[test]
    fn beta_10_capability_gate_does_not_match_beta_100() {
        assert!(is_rustfs_beta_10("1.0.0-beta.10"));
        assert!(is_rustfs_beta_10("rustfs/v1.0.0-beta.10+build.7"));
        assert!(!is_rustfs_beta_10("1.0.0-beta.100"));
        assert!(!is_rustfs_beta_10("1.0.0-beta.9"));
    }

    #[tokio::test]
    async fn client_devnull_streams_exact_signed_payloads_and_aggregates_measurements() {
        let (endpoint, receiver, handle) = start_admin_sequence_server(vec![
            ("200 OK", CAPABILITY_INFO_RESPONSE),
            ("200 OK", RUNTIME_CAPABILITIES_RESPONSE),
            ("200 OK", EXTENSIONS_RESPONSE),
            ("200 OK", CLUSTER_SNAPSHOT_RESPONSE),
            ("200 OK", CLIENT_DEVNULL_RESPONSE),
            ("200 OK", CLIENT_DEVNULL_RESPONSE),
        ]);
        let client = admin_client_for_endpoint(&endpoint);
        let request = ClientDevnullRequest::new(65_536, 2, Duration::from_secs(5))
            .expect("probe request should be valid");

        let result = client
            .client_devnull(request)
            .await
            .expect("client devnull should succeed");

        assert_eq!(result.requested_bytes, 131_072);
        assert_eq!(result.received_bytes, 131_072);
        assert_eq!(result.concurrency, 2);
        assert!(result.elapsed_seconds > 0.0);
        assert!(result.elapsed_seconds < 5.0);
        assert_eq!(
            result.aggregate_throughput_bytes_per_second,
            result.received_bytes as f64 / result.elapsed_seconds
        );

        let captured = (0..6)
            .map(|_| receiver.recv().expect("captured request"))
            .collect::<Vec<_>>();
        assert_eq!(
            captured
                .iter()
                .map(|request| request.target.as_str())
                .collect::<Vec<_>>(),
            vec![
                "/rustfs/admin/v3/info",
                "/rustfs/admin/v4/runtime/capabilities",
                "/rustfs/admin/v4/extensions/catalog",
                "/rustfs/admin/v4/cluster/snapshot",
                "/rustfs/admin/v3/speedtest/client/devnull",
                "/rustfs/admin/v3/speedtest/client/devnull",
            ]
        );
        for request in &captured[4..] {
            assert_eq!(request.method, "POST");
            assert_eq!(request.body.len(), 65_536);
            assert!(request.body.iter().all(|byte| *byte == 0));
            assert!(request.headers.contains("content-length: 65536"));
            assert!(request.headers.contains(&format!(
                "x-amz-content-sha256: {}",
                AdminClient::sha256_hash(&request.body)
            )));
        }
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn client_devnull_rejects_placeholder_and_mismatched_responses() {
        for response in [
            r#"{"kind":"client-devnull","measured":false,"aggregate_write_throughput_bytes_per_sec":2048.0,"rx_bytes":65536,"duration_secs":0.5}"#,
            r#"{"kind":"client-devnull","measured":true,"aggregate_write_throughput_bytes_per_sec":2048.0,"rx_bytes":1,"duration_secs":0.5}"#,
            r#"{"kind":"object","measured":true,"aggregate_write_throughput_bytes_per_sec":2048.0,"rx_bytes":65536,"duration_secs":0.5}"#,
            r#"{"kind":"client-devnull","measured":true,"aggregate_write_throughput_bytes_per_sec":0.0,"rx_bytes":65536,"duration_secs":0.5}"#,
            r#"{"kind":"client-devnull","measured":true,"aggregate_write_throughput_bytes_per_sec":2048.0,"rx_bytes":65536,"duration_secs":0.0}"#,
            r#"{"kind":"client-devnull","measured":true,"aggregate_write_throughput_bytes_per_sec":2048.0,"rx_bytes":65536}"#,
            r#"{"kind":"client-devnull","measured":true,"rx_bytes":65536,"duration_secs":0.5}"#,
        ] {
            let (endpoint, _receiver, handle) = start_admin_sequence_server(vec![
                ("200 OK", CAPABILITY_INFO_RESPONSE),
                ("200 OK", RUNTIME_CAPABILITIES_RESPONSE),
                ("200 OK", EXTENSIONS_RESPONSE),
                ("200 OK", CLUSTER_SNAPSHOT_RESPONSE),
                ("200 OK", response),
            ]);
            let client = admin_client_for_endpoint(&endpoint);
            let request = ClientDevnullRequest::new(65_536, 1, Duration::from_secs(5))
                .expect("probe request should be valid");

            let error = client
                .client_devnull(request)
                .await
                .expect_err("placeholder responses must not succeed");

            assert!(matches!(error, Error::UnsupportedFeature(_)));
            handle.join().expect("server thread should finish");
        }
    }

    #[tokio::test]
    async fn client_devnull_fails_closed_before_active_request_on_unknown_version() {
        let (endpoint, receiver, handle) = start_admin_sequence_server(vec![
            ("200 OK", UNKNOWN_CAPABILITY_INFO_RESPONSE),
            ("200 OK", RUNTIME_CAPABILITIES_RESPONSE),
            ("200 OK", EXTENSIONS_RESPONSE),
            ("200 OK", CLUSTER_SNAPSHOT_RESPONSE),
        ]);
        let client = admin_client_for_endpoint(&endpoint);
        let request = ClientDevnullRequest::new(65_536, 1, Duration::from_secs(5))
            .expect("probe request should be valid");

        let error = client
            .client_devnull(request)
            .await
            .expect_err("unknown capability must fail closed");

        assert!(matches!(error, Error::UnsupportedFeature(_)));
        let targets = (0..4)
            .map(|_| receiver.recv().expect("captured request").target)
            .collect::<Vec<_>>();
        assert!(!targets.iter().any(|target| target.contains("devnull")));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn client_devnull_preserves_permission_denial() {
        let (endpoint, _receiver, handle) = start_admin_sequence_server(vec![
            ("200 OK", CAPABILITY_INFO_RESPONSE),
            ("200 OK", RUNTIME_CAPABILITIES_RESPONSE),
            ("200 OK", EXTENSIONS_RESPONSE),
            ("200 OK", CLUSTER_SNAPSHOT_RESPONSE),
            ("403 Forbidden", r#"{"code":"AccessDenied"}"#),
        ]);
        let client = admin_client_for_endpoint(&endpoint);
        let request = ClientDevnullRequest::new(65_536, 1, Duration::from_secs(5))
            .expect("probe request should be valid");

        let error = client
            .client_devnull(request)
            .await
            .expect_err("permission denial must not be reclassified");

        assert!(matches!(error, Error::Auth(_)));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn client_devnull_timeout_stops_incremental_body_generation() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            for response_body in [
                CAPABILITY_INFO_RESPONSE,
                RUNTIME_CAPABILITIES_RESPONSE,
                EXTENSIONS_RESPONSE,
                CLUSTER_SNAPSHOT_RESPONSE,
            ] {
                let (mut stream, _) = listener.accept().expect("accept discovery request");
                let _request = read_admin_request(&mut stream);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write discovery response");
            }

            let (mut stream, _) = listener.accept().expect("accept active request");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set read timeout");
            let (_headers, content_length, mut received) = read_admin_request_head(&mut stream);
            thread::sleep(Duration::from_millis(1_200));
            let mut chunk = [0_u8; 8192];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => received += read,
                }
            }
            sender
                .send((content_length, received))
                .expect("send upload byte counts");
        });
        let client = admin_client_for_endpoint(&endpoint);
        let request = ClientDevnullRequest::new(64 * 1024 * 1024, 1, Duration::from_secs(1))
            .expect("probe request should be valid");

        let error = client
            .client_devnull(request)
            .await
            .expect_err("stalled upload must time out");

        assert!(matches!(error, Error::Network(message) if message.contains("timed out")));
        let (content_length, received) = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("upload byte counts");
        assert_eq!(content_length, 64 * 1024 * 1024);
        assert!(
            received < content_length,
            "received {received} of {content_length}"
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn client_devnull_does_not_retry_server_failures() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            for response_body in [
                CAPABILITY_INFO_RESPONSE,
                RUNTIME_CAPABILITIES_RESPONSE,
                EXTENSIONS_RESPONSE,
                CLUSTER_SNAPSHOT_RESPONSE,
            ] {
                let (mut stream, _) = listener.accept().expect("accept discovery request");
                let _request = read_admin_request(&mut stream);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write discovery response");
            }

            let (mut stream, _) = listener.accept().expect("accept active request");
            let request = read_admin_request(&mut stream);
            assert_eq!(request.target, "/rustfs/admin/v3/speedtest/client/devnull");
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\nconnection: close\r\n\r\nbusy",
                )
                .expect("write active response");

            listener
                .set_nonblocking(true)
                .expect("set listener nonblocking");
            let deadline = std::time::Instant::now() + Duration::from_millis(300);
            let mut active_requests = 1;
            while std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((_stream, _)) => active_requests += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept retry request: {error}"),
                }
            }
            sender
                .send(active_requests)
                .expect("send active request count");
        });
        let client = admin_client_for_endpoint(&endpoint);
        let request = ClientDevnullRequest::new(65_536, 1, Duration::from_secs(5))
            .expect("probe request should be valid");

        let error = client
            .client_devnull(request)
            .await
            .expect_err("server failure must be returned");

        assert!(matches!(error, Error::Network(_)));
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("active request count"),
            1
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn client_devnull_rejects_declared_and_chunked_response_overflow() {
        let declared = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            64 * 1024 + 1
        )
        .into_bytes();
        let first_chunk = vec![b'a'; 32 * 1024];
        let second_chunk = vec![b'b'; 32 * 1024 + 1];
        let mut chunked =
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n".to_vec();
        chunked.extend_from_slice(format!("{:x}\r\n", first_chunk.len()).as_bytes());
        chunked.extend_from_slice(&first_chunk);
        chunked.extend_from_slice(b"\r\n");
        chunked.extend_from_slice(format!("{:x}\r\n", second_chunk.len()).as_bytes());
        chunked.extend_from_slice(&second_chunk);
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");

        for raw_response in [declared, chunked] {
            let (endpoint, handle) = start_client_devnull_raw_response_server(raw_response);
            let client = admin_client_for_endpoint(&endpoint);
            let request = ClientDevnullRequest::new(65_536, 1, Duration::from_secs(5))
                .expect("probe request should be valid");

            let error = client
                .client_devnull(request)
                .await
                .expect_err("oversized responses must be rejected");

            assert!(
                matches!(error, Error::UnsupportedFeature(message) if message.contains("exceeded"))
            );
            handle.join().expect("server thread should finish");
        }
    }

    #[tokio::test]
    async fn aborting_client_devnull_closes_an_incomplete_upload() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let (accepted_sender, accepted_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (result_sender, result_receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            for response_body in [
                CAPABILITY_INFO_RESPONSE,
                RUNTIME_CAPABILITIES_RESPONSE,
                EXTENSIONS_RESPONSE,
                CLUSTER_SNAPSHOT_RESPONSE,
            ] {
                let (mut stream, _) = listener.accept().expect("accept discovery request");
                let _request = read_admin_request(&mut stream);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write discovery response");
            }

            let (mut stream, _) = listener.accept().expect("accept active request");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set read timeout");
            let (_headers, content_length, mut received) = read_admin_request_head(&mut stream);
            accepted_sender.send(()).expect("signal active request");
            release_receiver.recv().expect("wait for cancellation");
            let mut chunk = [0_u8; 8192];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => received += read,
                }
            }
            result_sender
                .send((content_length, received))
                .expect("send cancellation result");
        });
        let client = admin_client_for_endpoint(&endpoint);
        let request = ClientDevnullRequest::new(64 * 1024 * 1024, 1, Duration::from_secs(60))
            .expect("probe request should be valid");
        let task = tokio::spawn(async move { client.client_devnull(request).await });
        tokio::task::spawn_blocking(move || {
            accepted_receiver
                .recv_timeout(Duration::from_secs(15))
                .expect("active request accepted")
        })
        .await
        .expect("accept wait task");

        task.abort();
        let join_error = task.await.expect_err("aborted task must not complete");
        assert!(join_error.is_cancelled());
        release_sender.send(()).expect("release test server");

        let (content_length, received) = result_receiver
            .recv_timeout(Duration::from_secs(15))
            .expect("cancellation result");
        assert_eq!(content_length, 64 * 1024 * 1024);
        assert!(
            received < content_length,
            "received {received} of {content_length}"
        );
        handle.join().expect("server thread should finish");
    }

    #[test]
    fn client_devnull_chunks_and_hash_are_bounded_and_exact() {
        let mut remaining = 2 * CLIENT_DEVNULL_CHUNK_BYTES as u64 + 17;
        let mut generated = 0_u64;
        while let Some((chunk, next_remaining)) = next_client_devnull_chunk(remaining) {
            assert!(chunk.len() <= CLIENT_DEVNULL_CHUNK_BYTES);
            assert!(chunk.iter().all(|byte| *byte == 0));
            generated += chunk.len() as u64;
            remaining = next_remaining;
        }

        assert_eq!(generated, 2 * CLIENT_DEVNULL_CHUNK_BYTES as u64 + 17);
        assert_eq!(
            client_devnull_payload_hash(3),
            AdminClient::sha256_hash(&[0, 0, 0])
        );
    }

    #[tokio::test]
    async fn capability_discovery_refreshes_process_shared_cache() {
        let responses = vec![
            ("200 OK", CAPABILITY_INFO_RESPONSE),
            ("200 OK", RUNTIME_CAPABILITIES_RESPONSE),
            ("200 OK", EXTENSIONS_RESPONSE),
            ("200 OK", CLUSTER_SNAPSHOT_RESPONSE),
            ("200 OK", CAPABILITY_INFO_RESPONSE),
            ("200 OK", RUNTIME_CAPABILITIES_RESPONSE),
            ("200 OK", EXTENSIONS_RESPONSE),
            ("200 OK", CLUSTER_SNAPSHOT_RESPONSE),
        ];
        let (endpoint, receiver, handle) = start_admin_sequence_server(responses);
        let first_client = admin_client_for_endpoint(&endpoint);
        first_client
            .discover_capabilities(false)
            .await
            .expect("initial discovery should succeed");

        let second_client = admin_client_for_endpoint(&endpoint);
        second_client
            .discover_capabilities(true)
            .await
            .expect("refresh should bypass the process-shared cache");

        let targets = (0..8)
            .map(|_| receiver.recv().expect("captured request").target)
            .collect::<Vec<_>>();
        assert_eq!(
            targets,
            [
                "/rustfs/admin/v3/info",
                "/rustfs/admin/v4/runtime/capabilities",
                "/rustfs/admin/v4/extensions/catalog",
                "/rustfs/admin/v4/cluster/snapshot",
            ]
            .repeat(2)
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn capability_cache_isolates_tls_contexts_and_refreshes_within_each_context() {
        let response_set = [
            ("200 OK", CAPABILITY_INFO_RESPONSE),
            ("200 OK", RUNTIME_CAPABILITIES_RESPONSE),
            ("200 OK", EXTENSIONS_RESPONSE),
            ("200 OK", CLUSTER_SNAPSHOT_RESPONSE),
        ];
        let responses = response_set.repeat(3);
        let (endpoint, receiver, handle) = start_admin_sequence_server(responses);

        let strict_client = admin_client_for_endpoint(&endpoint);
        strict_client
            .discover_capabilities(false)
            .await
            .expect("strict TLS context discovery should succeed");

        let mut insecure_alias = Alias::new("test", &endpoint, "access", "secret");
        insecure_alias.insecure = true;
        let insecure_client =
            AdminClient::new(&insecure_alias).expect("insecure admin client should build");
        assert_ne!(
            strict_client.capability_cache_key(),
            insecure_client.capability_cache_key()
        );
        insecure_client
            .discover_capabilities(false)
            .await
            .expect("a different TLS context must not reuse the strict cache entry");

        let second_insecure_client =
            AdminClient::new(&insecure_alias).expect("second insecure client should build");
        second_insecure_client
            .discover_capabilities(false)
            .await
            .expect("the same TLS context should reuse its cache entry");
        second_insecure_client
            .discover_capabilities(true)
            .await
            .expect("refresh should bypass the matching TLS context cache entry");

        let targets = (0..12)
            .map(|_| receiver.recv().expect("captured request").target)
            .collect::<Vec<_>>();
        assert_eq!(
            targets,
            [
                "/rustfs/admin/v3/info",
                "/rustfs/admin/v4/runtime/capabilities",
                "/rustfs/admin/v4/extensions/catalog",
                "/rustfs/admin/v4/cluster/snapshot",
            ]
            .repeat(3)
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn capability_discovery_404_takes_precedence_over_not_implemented_body() {
        let (endpoint, receiver, handle) = start_admin_sequence_server(vec![
            ("200 OK", CAPABILITY_INFO_RESPONSE),
            (
                "404 Not Found",
                r#"{"code":"NotImplemented","message":"route body must not override HTTP 404"}"#,
            ),
        ]);
        let client = admin_client_for_endpoint(&endpoint);

        let report = client
            .discover_capabilities(false)
            .await
            .expect("v4 absence should produce a report");

        assert_eq!(report.capabilities.len(), 11);
        assert!(
            report.capabilities.iter().all(|capability| {
                capability.availability == CapabilityAvailability::VersionGated
            })
        );
        assert_eq!(
            receiver.recv().expect("info request").target,
            "/rustfs/admin/v3/info"
        );
        assert_eq!(
            receiver.recv().expect("runtime request").target,
            "/rustfs/admin/v4/runtime/capabilities"
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn capability_discovery_rejects_oversized_v4_json() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept info request");
            let _request = read_admin_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{}",
                CAPABILITY_INFO_RESPONSE.len(),
                CAPABILITY_INFO_RESPONSE
            );
            stream
                .write_all(response.as_bytes())
                .expect("write info response");

            let (mut stream, _) = listener.accept().expect("accept runtime request");
            let _request = read_admin_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n",
                rc_core::admin::MAX_DIAGNOSTIC_RESPONSE_BYTES + 1
            );
            stream
                .write_all(response.as_bytes())
                .expect("write oversized response headers");
        });
        let client = admin_client_for_endpoint(&endpoint);

        let error = client
            .discover_capabilities(false)
            .await
            .expect_err("oversized runtime discovery should fail");

        assert!(matches!(error, Error::General(message) if message.contains("8 MiB")));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn capability_discovery_reports_http_501_as_stubbed() {
        let (endpoint, receiver, handle) = start_admin_sequence_server(vec![
            ("200 OK", CAPABILITY_INFO_RESPONSE),
            (
                "501 Not Implemented",
                r#"{"code":"NotImplemented","message":"route is not implemented"}"#,
            ),
        ]);
        let client = admin_client_for_endpoint(&endpoint);

        let report = client
            .discover_capabilities(false)
            .await
            .expect("an explicit stub response should produce a report");

        assert_eq!(report.capabilities.len(), 11);
        assert!(report.capabilities.iter().any(|capability| {
            capability.name == "admin.runtime-capabilities"
                && capability.availability == CapabilityAvailability::Stubbed
                && capability.reason.as_deref() == Some("route is not implemented")
        }));
        assert!(
            report
                .capabilities
                .iter()
                .filter(|capability| {
                    capability.name.starts_with("admin.diagnostics.")
                        || capability.name == "admin.site-replication.netperf"
                })
                .all(|capability| capability.availability == CapabilityAvailability::Unknown)
        );
        assert_eq!(
            receiver.recv().expect("info request").target,
            "/rustfs/admin/v3/info"
        );
        assert_eq!(
            receiver.recv().expect("runtime request").target,
            "/rustfs/admin/v4/runtime/capabilities"
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn capability_discovery_does_not_misclassify_permission_denial() {
        let (endpoint, _receiver, handle) = start_admin_sequence_server(vec![
            ("200 OK", CAPABILITY_INFO_RESPONSE),
            ("403 Forbidden", r#"{"code":"AccessDenied"}"#),
        ]);
        let client = admin_client_for_endpoint(&endpoint);

        let error = client
            .discover_capabilities(false)
            .await
            .expect_err("permission denial should fail discovery");

        assert!(matches!(error, Error::Auth(_)));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn capability_discovery_maps_rustfs_missing_credentials_to_auth() {
        let (endpoint, _receiver, handle) = start_admin_sequence_server(vec![
            ("200 OK", CAPABILITY_INFO_RESPONSE),
            (
                "400 Bad Request",
                "<Error><Code>InvalidRequest</Code><Message>get cred failed</Message></Error>",
            ),
        ]);
        let client = anonymous_admin_client_for_endpoint(&endpoint);

        let error = client
            .discover_capabilities(false)
            .await
            .expect_err("RustFS missing credentials should fail discovery");

        assert!(matches!(error, Error::Auth(_)));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn capability_discovery_rejects_malformed_runtime_response() {
        let (endpoint, _receiver, handle) = start_admin_sequence_server(vec![
            ("200 OK", CAPABILITY_INFO_RESPONSE),
            ("200 OK", r#"{"summary":{}}"#),
        ]);
        let client = admin_client_for_endpoint(&endpoint);

        let error = client
            .discover_capabilities(false)
            .await
            .expect_err("malformed response should fail discovery");

        assert!(matches!(error, Error::Json(_)));
        handle.join().expect("server thread should finish");
    }

    #[test]
    fn not_implemented_admin_error_maps_to_unsupported_feature() {
        let client = admin_client_for_endpoint("http://localhost:9000");
        let error = client.map_error(
            StatusCode::NOT_IMPLEMENTED,
            r#"{"code":"NotImplemented","message":"route is not implemented"}"#,
        );

        assert!(matches!(error, Error::UnsupportedFeature(_)));
    }

    #[test]
    fn structured_not_implemented_code_maps_to_unsupported_feature() {
        let client = admin_client_for_endpoint("http://localhost:9000");
        let error = client.map_error(
            StatusCode::BAD_REQUEST,
            "<Error><Code>NotImplemented</Code><Message>stub route</Message></Error>",
        );

        assert!(matches!(error, Error::UnsupportedFeature(message) if message == "stub route"));
    }

    #[test]
    fn permission_status_takes_precedence_over_not_implemented_code() {
        let client = admin_client_for_endpoint("http://localhost:9000");
        let error = client.map_error(
            StatusCode::FORBIDDEN,
            r#"{"code":"NotImplemented","message":"Access denied"}"#,
        );

        assert!(matches!(error, Error::Auth(_)));
    }

    #[test]
    fn not_found_status_takes_precedence_over_not_implemented_code() {
        let client = admin_client_for_endpoint("http://localhost:9000");
        let error = client.map_error(
            StatusCode::NOT_FOUND,
            r#"{"code":"NotImplemented","message":"route is not implemented"}"#,
        );

        assert!(matches!(error, Error::NotFound(_)));
    }

    #[test]
    fn unstructured_not_implemented_text_does_not_change_error_class() {
        let client = admin_client_for_endpoint("http://localhost:9000");
        let error = client.map_error(
            StatusCode::BAD_REQUEST,
            "A proxy says NotImplemented but provides no structured error code",
        );

        assert!(matches!(error, Error::General(_)));
    }

    #[test]
    fn rustfs_missing_credentials_invalid_request_maps_to_auth() {
        let client = admin_client_for_endpoint("http://localhost:9000");
        for body in [
            "<Error><Code>InvalidRequest</Code><Message>get cred failed</Message></Error>",
            r#"{"Code":"InvalidRequest","Message":"authentication required"}"#,
        ] {
            let error = client.map_error(StatusCode::BAD_REQUEST, body);
            assert!(matches!(error, Error::Auth(_)));
        }
    }

    #[tokio::test]
    async fn iam_policy_entities_uses_repeated_typed_filters_and_drops_unknown_secrets() {
        let response = r#"{
            "timestamp":"2026-07-24T08:00:00Z",
            "userMappings":[{
                "user":"alice",
                "policies":["readonly"],
                "memberOfMappings":[{"group":"ops","policies":["diagnostics"]}],
                "secretKey":"server-secret"
            }],
            "groupMappings":[{"group":"ops","policies":["diagnostics"]}],
            "policyMappings":[{"policy":"readonly","users":["alice"],"groups":[]}],
            "sessionToken":"server-token"
        }"#;
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", response);
        let client = admin_client_for_endpoint(&endpoint);
        let result = client
            .policy_entities(&PolicyEntitiesQuery {
                users: vec!["alice@example.com".to_string(), "bob".to_string()],
                groups: vec!["ops/team".to_string()],
                policies: vec!["read only".to_string()],
            })
            .await
            .expect("policy entities request");

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/idp/builtin/policy-entities?user=alice%40example.com&user=bob&group=ops%2Fteam&policy=read%20only"
        );
        assert!(request.body.is_empty());
        assert_eq!(result.user_mappings[0].user, "alice");
        let encoded = serde_json::to_string(&result).expect("serialize typed result");
        assert!(!encoded.contains("server-secret"));
        assert!(!encoded.contains("server-token"));
        handle.join().expect("join policy entities server");
    }

    #[tokio::test]
    async fn iam_policy_entities_maps_route_absence_and_permission_without_echoing_body() {
        for (status, body, expected) in [
            (
                "404 Not Found",
                r#"{"Code":"NoSuchRoute","Message":"access secret leaked"}"#,
                "unsupported",
            ),
            (
                "405 Method Not Allowed",
                "access secret leaked",
                "unsupported",
            ),
            ("501 Not Implemented", "access secret leaked", "unsupported"),
            ("403 Forbidden", "access secret leaked", "auth"),
        ] {
            let (endpoint, _receiver, handle) = start_admin_test_server(status, body);
            let client = admin_client_for_endpoint(&endpoint);
            let error = client
                .policy_entities(&PolicyEntitiesQuery::default())
                .await
                .expect_err("request must fail");

            match expected {
                "unsupported" => assert!(matches!(error, Error::UnsupportedFeature(_))),
                "auth" => assert!(matches!(error, Error::Auth(_))),
                _ => unreachable!(),
            }
            assert!(!error.to_string().contains("access secret leaked"));
            handle.join().expect("join policy entities error server");
        }
    }

    #[tokio::test]
    async fn iam_policy_entities_rejects_malformed_and_oversized_responses() {
        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", "{");
        let client = admin_client_for_endpoint(&endpoint);
        let malformed = client
            .policy_entities(&PolicyEntitiesQuery::default())
            .await
            .expect_err("malformed JSON must fail");
        assert!(matches!(malformed, Error::Json(_)));
        handle.join().expect("join malformed response server");

        let (endpoint, _receiver, handle) = start_admin_declared_length_server(
            "200 OK",
            MAX_IAM_POLICY_ENTITIES_RESPONSE_BYTES + 1,
        );
        let client = admin_client_for_endpoint(&endpoint);
        let oversized = client
            .policy_entities(&PolicyEntitiesQuery::default())
            .await
            .expect_err("oversized response must fail");
        assert!(
            matches!(oversized, Error::General(message) if message.contains("IAM policy-entity response exceeded"))
        );
        handle.join().expect("join oversized response server");
    }

    #[tokio::test]
    async fn iam_policy_detach_sends_one_typed_target_and_normalizes_multi_policy_result() {
        let response = r#"{
            "policiesAttached":[],
            "policiesDetached":["readonly","diagnostics","readonly"],
            "updatedAt":"2026-07-24T08:00:00Z"
        }"#;
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", response);
        let client = admin_client_for_endpoint(&endpoint);
        let request = PolicyDetachRequest::new(
            vec![
                "writeonly".to_string(),
                "readonly".to_string(),
                "diagnostics".to_string(),
            ],
            PolicyDetachEntity::Group,
            "ops".to_string(),
        )
        .expect("valid detach request");
        let result = client
            .detach_policies(&request)
            .await
            .expect("policy detach request");

        let captured = receiver.recv().expect("captured request");
        assert_eq!(captured.method, "POST");
        assert_eq!(
            captured.target,
            "/rustfs/admin/v3/idp/builtin/policy/detach"
        );
        let body: serde_json::Value =
            serde_json::from_slice(&captured.body).expect("typed JSON request");
        assert_eq!(body["group"], "ops");
        assert!(body.get("user").is_none());
        assert_eq!(
            body["policies"],
            serde_json::json!(["diagnostics", "readonly", "writeonly"])
        );
        assert_eq!(result.detached, ["diagnostics", "readonly"]);
        assert_eq!(result.unchanged, ["writeonly"]);
        assert!(result.attached.is_empty());
        assert_eq!(result.entity, PolicyDetachEntity::Group);
        assert_eq!(result.entity_name, "ops");
        handle.join().expect("join policy detach server");
    }

    #[tokio::test]
    async fn iam_policy_detach_noop_retry_reports_every_requested_policy_unchanged() {
        let response = r#"{"policiesDetached":[],"updatedAt":"2026-07-24T08:00:00Z"}"#;
        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", response);
        let client = admin_client_for_endpoint(&endpoint);
        let request = PolicyDetachRequest::new(
            vec!["readonly".to_string(), "diagnostics".to_string()],
            PolicyDetachEntity::User,
            "alice".to_string(),
        )
        .expect("valid detach request");
        let result = client
            .detach_policies(&request)
            .await
            .expect("idempotent no-op detach");

        assert!(result.detached.is_empty());
        assert_eq!(result.unchanged, ["diagnostics", "readonly"]);
        handle.join().expect("join policy detach server");
    }

    #[tokio::test]
    async fn iam_policy_detach_classifies_missing_entity_and_access_denial() {
        for (status, body, expected) in [
            (
                "400 Bad Request",
                r#"{"Code":"InvalidArgument","Message":"user not exist"}"#,
                "missing",
            ),
            (
                "403 Forbidden",
                r#"{"Code":"AccessDenied","Message":"do not echo this"}"#,
                "auth",
            ),
        ] {
            let (endpoint, _receiver, handle) = start_admin_test_server(status, body);
            let client = admin_client_for_endpoint(&endpoint);
            let request = PolicyDetachRequest::new(
                vec!["readonly".to_string()],
                PolicyDetachEntity::User,
                "missing".to_string(),
            )
            .expect("valid detach request");
            let error = client
                .detach_policies(&request)
                .await
                .expect_err("detach should fail");
            match expected {
                "missing" => assert!(matches!(error, Error::NotFound(_))),
                "auth" => assert!(matches!(error, Error::Auth(_))),
                _ => unreachable!(),
            }
            assert!(!error.to_string().contains("do not echo this"));
            handle.join().expect("join policy detach error server");
        }
    }

    #[tokio::test]
    async fn bulk_access_keys_classifies_mixed_types_and_drops_secret_canaries() {
        let response = r#"{
            "alice":{
                "serviceAccounts":[{
                    "accessKey":"svc-b",
                    "parentUser":"alice",
                    "accountStatus":"on",
                    "expiration":"2027-01-01T00:00:00Z",
                    "impliedPolicy":true,
                    "secretKey":"must-not-survive"
                }],
                "stsKeys":[{
                    "accessKey":"sts-a",
                    "parentUser":"alice",
                    "sessionToken":"must-not-survive"
                }]
            }
        }"#;
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", response);
        let client = admin_client_for_endpoint(&endpoint);
        let result = client
            .list_access_keys_bulk(&BulkAccessKeyQuery {
                provider: AccessKeyProvider::Builtin,
                users: vec!["alice@example.com".to_string(), "bob".to_string()],
                all: false,
                list_type: rc_core::admin::AccessKeyListType::All,
            })
            .await
            .expect("bulk access-key request");

        let request = receiver.recv().expect("captured request");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/list-access-keys-bulk?users=alice%40example.com&users=bob&listType=all"
        );
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].kind, AccessKeyKind::ServiceAccount);
        assert_eq!(result[1].kind, AccessKeyKind::Sts);
        assert!(result.iter().all(|record| {
            record.provider == AccessKeyProvider::Builtin && record.parent_user == "alice"
        }));
        let encoded = serde_json::to_string(&result).expect("serialize safe records");
        assert!(!encoded.contains("must-not-survive"));
        assert!(!encoded.contains("secretKey"));
        assert!(!encoded.contains("sessionToken"));
        handle.join().expect("join bulk access-key server");
    }

    #[tokio::test]
    async fn bulk_access_keys_uses_advertised_federated_routes() {
        for (provider, expected_path) in [
            (
                AccessKeyProvider::Ldap,
                "/rustfs/admin/v3/idp/ldap/list-access-keys-bulk?all=true&listType=svcacc-only",
            ),
            (
                AccessKeyProvider::Openid,
                "/rustfs/admin/v3/idp/openid/list-access-keys-bulk?all=true&listType=svcacc-only",
            ),
        ] {
            let (endpoint, receiver, handle) =
                start_admin_test_server("200 OK", r#"{"parent":{"serviceAccounts":[]}}"#);
            let client = admin_client_for_endpoint(&endpoint);
            let result = client
                .list_access_keys_bulk(&BulkAccessKeyQuery {
                    provider,
                    users: Vec::new(),
                    all: true,
                    list_type: rc_core::admin::AccessKeyListType::ServiceAccountsOnly,
                })
                .await
                .expect("federated bulk request");

            assert!(result.is_empty());
            assert_eq!(
                receiver.recv().expect("captured request").target,
                expected_path
            );
            handle.join().expect("join federated route server");
        }
    }

    #[tokio::test]
    async fn iam_policy_detach_rejects_malformed_and_oversized_responses() {
        let request = PolicyDetachRequest::new(
            vec!["readonly".to_string()],
            PolicyDetachEntity::User,
            "alice".to_string(),
        )
        .expect("valid detach request");

        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", "{");
        let malformed = admin_client_for_endpoint(&endpoint)
            .detach_policies(&request)
            .await
            .expect_err("malformed response must fail");
        assert!(
            matches!(malformed, Error::General(message) if message.contains("malformed policy detach response"))
        );
        handle.join().expect("join malformed detach server");

        let (endpoint, _receiver, handle) =
            start_admin_declared_length_server("200 OK", MAX_IAM_POLICY_DETACH_RESPONSE_BYTES + 1);
        let oversized = admin_client_for_endpoint(&endpoint)
            .detach_policies(&request)
            .await
            .expect_err("oversized response must fail");
        assert!(
            matches!(oversized, Error::General(message) if message.contains("IAM policy detach response exceeded"))
        );
        handle.join().expect("join oversized detach server");
    }

    #[tokio::test]
    async fn iam_policy_detach_rejects_oversized_request_before_network_access() {
        let policies = (0..rc_core::admin::MAX_IAM_POLICY_DETACH_POLICIES)
            .map(|index| {
                format!(
                    "{index:04}-{}",
                    "p".repeat(rc_core::admin::MAX_IAM_POLICY_DETACH_SELECTOR_BYTES - 5)
                )
            })
            .collect();
        let request =
            PolicyDetachRequest::new(policies, PolicyDetachEntity::User, "alice".to_string())
                .expect("individual selectors fit their limits");
        let error = admin_client_for_endpoint("http://127.0.0.1:9")
            .detach_policies(&request)
            .await
            .expect_err("oversized request must fail locally");
        assert!(
            matches!(error, Error::InvalidPath(message) if message.contains("request exceeds"))
        );
    }

    #[tokio::test]
    async fn bulk_access_keys_distinguishes_malformed_denied_unsupported_and_oversized() {
        for (status, body, expected) in [
            ("200 OK", "{", "malformed"),
            ("403 Forbidden", "secret server body", "denied"),
            ("501 Not Implemented", "secret server body", "unsupported"),
        ] {
            let (endpoint, _receiver, handle) = start_admin_test_server(status, body);
            let client = admin_client_for_endpoint(&endpoint);
            let error = client
                .list_access_keys_bulk(&BulkAccessKeyQuery::default())
                .await
                .expect_err("bulk request should fail");
            match expected {
                "malformed" => assert!(matches!(error, Error::General(_))),
                "denied" => assert!(matches!(error, Error::Auth(_))),
                "unsupported" => assert!(matches!(error, Error::UnsupportedFeature(_))),
                _ => unreachable!(),
            }
            assert!(!error.to_string().contains("secret server body"));
            handle.join().expect("join bulk error server");
        }

        let (endpoint, _receiver, handle) =
            start_admin_declared_length_server("200 OK", MAX_IAM_ACCESS_KEYS_RESPONSE_BYTES + 1);
        let client = admin_client_for_endpoint(&endpoint);
        let oversized = client
            .list_access_keys_bulk(&BulkAccessKeyQuery::default())
            .await
            .expect_err("oversized bulk response must fail");
        assert!(
            matches!(oversized, Error::General(message) if message.contains("bulk access-key response exceeded"))
        );
        handle.join().expect("join oversized bulk response server");
    }

    #[test]
    fn capability_cache_key_is_credential_scoped_without_plaintext_secrets() {
        let first = admin_client_for_endpoint("http://localhost:9000");
        let mut alias = Alias::new(
            "test",
            "http://localhost:9000",
            "different-access",
            "different-secret",
        );
        alias.region = first.region.clone();
        let second = AdminClient::new(&alias).expect("admin client should build");

        let first_key = first.capability_cache_key();
        let second_key = second.capability_cache_key();
        assert_ne!(first_key, second_key);
        assert!(!first_key.credential_fingerprint.contains("secret"));
        assert!(
            !second_key
                .credential_fingerprint
                .contains("different-secret")
        );
    }

    #[test]
    fn transport_security_fingerprint_covers_tls_and_mtls_inputs_without_leaking_them() {
        let baseline = transport_security_fingerprint(false, None, None, None);
        let insecure = transport_security_fingerprint(true, None, None, None);
        let custom_ca = transport_security_fingerprint(false, Some(b"private-ca-root"), None, None);
        let client_identity = transport_security_fingerprint(
            false,
            Some(b"private-ca-root"),
            Some(b"client-certificate"),
            Some(b"PRIVATE-KEY-MARKER"),
        );
        let different_key = transport_security_fingerprint(
            false,
            Some(b"private-ca-root"),
            Some(b"client-certificate"),
            Some(b"DIFFERENT-PRIVATE-KEY"),
        );

        assert_ne!(baseline, insecure);
        assert_ne!(baseline, custom_ca);
        assert_ne!(custom_ca, client_identity);
        assert_ne!(client_identity, different_key);
        for fingerprint in [
            baseline,
            insecure,
            custom_ca,
            client_identity,
            different_key,
        ] {
            assert_eq!(fingerprint.len(), 64);
            assert!(
                fingerprint
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
            );
            assert!(!fingerprint.contains("PRIVATE-KEY-MARKER"));
            assert!(!fingerprint.contains("private-ca-root"));
        }
    }

    #[test]
    fn test_admin_url_with_trailing_slash() {
        let alias = Alias::new("test", "http://localhost:9000/", "access", "secret");
        let client = AdminClient::new(&alias).unwrap();

        assert_eq!(
            client.admin_url("/list-users"),
            "http://localhost:9000/rustfs/admin/v3/list-users"
        );
    }

    #[test]
    fn test_get_host() {
        let alias = Alias::new("test", "https://s3.example.com", "access", "secret");
        let client = AdminClient::new(&alias).unwrap();

        assert_eq!(client.get_host(), "s3.example.com");
    }

    #[test]
    fn test_sha256_hash() {
        let hash = AdminClient::sha256_hash(b"test");
        assert_eq!(
            hash,
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }

    #[test]
    fn test_sha256_hash_empty() {
        let hash = AdminClient::sha256_hash(b"");
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_rustfs_heal_path_matches_admin_routes() {
        assert_eq!(
            rustfs_heal_path(&HealStartRequest::default()).expect("root path"),
            "/heal/"
        );

        let bucket_request = HealStartRequest {
            bucket: Some("photos".to_string()),
            ..Default::default()
        };
        assert_eq!(
            rustfs_heal_path(&bucket_request).expect("bucket path"),
            "/heal/photos"
        );

        let prefix_request = HealStartRequest {
            bucket: Some("photos".to_string()),
            prefix: Some("2026/raw".to_string()),
            ..Default::default()
        };
        assert_eq!(
            rustfs_heal_path(&prefix_request).expect("prefix path"),
            "/heal/photos/2026%2Fraw"
        );

        let invalid_request = HealStartRequest {
            prefix: Some("2026/raw".to_string()),
            ..Default::default()
        };
        assert!(matches!(
            rustfs_heal_path(&invalid_request),
            Err(Error::InvalidPath(_))
        ));
    }

    #[test]
    fn test_rustfs_heal_task_path_supports_root_target() {
        let request = HealTaskRequest {
            bucket: String::new(),
            prefix: None,
            client_token: "root-token".to_string(),
        };

        assert_eq!(
            rustfs_heal_task_path(&request).expect("root task path"),
            "/heal/"
        );
    }

    #[test]
    fn test_rustfs_heal_task_path_rejects_root_prefix() {
        let request = HealTaskRequest {
            bucket: String::new(),
            prefix: Some("2026/".to_string()),
            client_token: "root-token".to_string(),
        };

        assert!(matches!(
            rustfs_heal_task_path(&request),
            Err(Error::InvalidPath(_))
        ));
    }

    #[test]
    fn test_rustfs_heal_body_matches_server_heal_options() {
        let request = HealStartRequest {
            scan_mode: HealScanMode::Deep,
            remove: true,
            recreate: true,
            dry_run: true,
            ..Default::default()
        };

        let body = rustfs_heal_body(&request).expect("heal options should serialize");
        let value: serde_json::Value =
            serde_json::from_slice(&body).expect("heal options body should be JSON");

        assert_eq!(value["recursive"], false);
        assert_eq!(value["dryRun"], true);
        assert_eq!(value["remove"], true);
        assert_eq!(value["recreate"], true);
        assert_eq!(value["scanMode"], 2);
        assert_eq!(value["updateParity"], false);
        assert_eq!(value["nolock"], false);
        assert!(value.get("bucket").is_none());
        assert!(value.get("prefix").is_none());
    }

    #[test]
    fn test_background_heal_status_response_maps_to_heal_status() {
        let status = HealStatus::from(BackgroundHealStatusResponse {
            state: Some(HealRuntimeState::Active),
            bitrot_start_time: Some("2026-04-19T10:00:00Z".to_string()),
            bitrot_start_cycle: 42,
            current_scan_mode: Some(2),
            heal_queue_length: 3,
            heal_active_tasks: 1,
            heal_operations: None,
            progress: None,
        });

        assert!(status.healing);
        assert_eq!(status.state, Some(HealRuntimeState::Active));
        assert_eq!(status.started.as_deref(), Some("2026-04-19T10:00:00Z"));
        assert_eq!(status.scan_mode, Some(HealScanMode::Deep));
        assert_eq!(status.scan_cycle, 42);
        assert_eq!(status.heal_queue_length, 3);
        assert_eq!(status.heal_active_tasks, 1);

        let idle = HealStatus::from(BackgroundHealStatusResponse {
            state: Some(HealRuntimeState::Idle),
            bitrot_start_time: None,
            bitrot_start_cycle: 0,
            current_scan_mode: Some(1),
            heal_queue_length: 0,
            heal_active_tasks: 0,
            heal_operations: None,
            progress: None,
        });
        assert!(!idle.healing);
        assert_eq!(idle.state, Some(HealRuntimeState::Idle));
        assert_eq!(idle.scan_mode, Some(HealScanMode::Normal));
        assert!(idle.started.is_none());

        let active_without_legacy_counters = HealStatus::from(BackgroundHealStatusResponse {
            state: Some(HealRuntimeState::Active),
            bitrot_start_time: None,
            bitrot_start_cycle: 0,
            current_scan_mode: Some(1),
            heal_queue_length: 0,
            heal_active_tasks: 0,
            heal_operations: None,
            progress: None,
        });
        assert!(active_without_legacy_counters.healing);

        let disabled_with_stale_counters = HealStatus::from(BackgroundHealStatusResponse {
            state: Some(HealRuntimeState::Disabled),
            bitrot_start_time: Some("2026-04-19T10:00:00Z".to_string()),
            bitrot_start_cycle: 42,
            current_scan_mode: Some(2),
            heal_queue_length: 3,
            heal_active_tasks: 1,
            heal_operations: None,
            progress: None,
        });
        assert!(!disabled_with_stale_counters.healing);

        let completed = HealStatus::from(BackgroundHealStatusResponse {
            state: None,
            bitrot_start_time: Some("2026-04-19T10:00:00Z".to_string()),
            bitrot_start_cycle: 42,
            current_scan_mode: Some(1),
            heal_queue_length: 0,
            heal_active_tasks: 0,
            heal_operations: None,
            progress: None,
        });
        assert!(!completed.healing);
        assert_eq!(completed.scan_mode, Some(HealScanMode::Normal));
        assert_eq!(completed.started.as_deref(), Some("2026-04-19T10:00:00Z"));

        let legacy = HealStatus::from(BackgroundHealStatusResponse {
            state: None,
            bitrot_start_time: Some("2026-04-19T10:00:00Z".to_string()),
            bitrot_start_cycle: 0,
            current_scan_mode: None,
            heal_queue_length: 0,
            heal_active_tasks: 0,
            heal_operations: None,
            progress: None,
        });
        assert!(legacy.healing);

        let active = HealStatus::from(BackgroundHealStatusResponse {
            state: None,
            bitrot_start_time: None,
            bitrot_start_cycle: 0,
            current_scan_mode: None,
            heal_queue_length: 0,
            heal_active_tasks: 1,
            heal_operations: None,
            progress: None,
        });
        assert!(active.healing);
    }

    #[test]
    fn test_background_heal_status_response_maps_nested_heal_operations() {
        let response: BackgroundHealStatusResponse = serde_json::from_str(
            r#"{"healOperations":{"queueLength":4,"activeTasks":1,"queuedBySource":{"admin":4}}}"#,
        )
        .expect("background heal status response should deserialize");

        let status = HealStatus::from(response);

        assert!(status.healing);
        assert_eq!(status.heal_queue_length, 4);
        assert_eq!(status.heal_active_tasks, 1);
    }

    #[test]
    fn test_background_heal_status_response_maps_progress() {
        let response: BackgroundHealStatusResponse = serde_json::from_str(
            r#"{"progress":{"objectsScanned":7,"objectsHealed":3,"objectsFailed":1,"bytesProcessed":4096,"bytesHealed":1024}}"#,
        )
        .expect("background heal status response should deserialize");

        let status = HealStatus::from(response);

        assert_eq!(status.items_scanned, 7);
        assert_eq!(status.items_healed, 3);
        assert_eq!(status.items_failed, 1);
        assert_eq!(status.bytes_scanned, 4096);
        assert_eq!(status.bytes_healed, 1024);
    }

    #[test]
    fn test_bad_request_maps_to_general_admin_error() {
        let alias = Alias::new("test", "http://localhost:9000", "access", "secret");
        let client = AdminClient::new(&alias).expect("admin client should build");

        let error = client.map_error(StatusCode::BAD_REQUEST, "err request body parse");
        assert!(matches!(error, Error::General(_)));
        assert_eq!(error.to_string(), "Bad request: err request body parse");
    }

    #[tokio::test]
    async fn test_heal_status_uses_background_heal_status_endpoint() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"bitrotStartTime":"2026-04-19T10:00:00Z","bitrotStartCycle":42,"currentScanMode":2,"healQueueLength":3,"healActiveTasks":1}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let status = client.heal_status().await.expect("heal status request");

        assert!(status.healing);
        assert_eq!(status.started.as_deref(), Some("2026-04-19T10:00:00Z"));
        assert_eq!(status.scan_mode, Some(HealScanMode::Deep));
        assert_eq!(status.scan_cycle, 42);
        assert_eq!(status.heal_queue_length, 3);
        assert_eq!(status.heal_active_tasks, 1);

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/rustfs/admin/v3/background-heal/status");
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_get_access_key_info_uses_info_access_key_endpoint() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"accessKey":"svc-ldap","userType":"Service Account","userProvider":"ldap","parentUser":"ldap-parent","accountStatus":"on","ldapSpecificInfo":{"username":"alice"}}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let info = client
            .get_access_key_info("svc-ldap")
            .await
            .expect("access key info request");

        assert_eq!(info.access_key, "svc-ldap");
        assert_eq!(info.user_type, "Service Account");
        assert_eq!(info.user_provider, "ldap");
        assert_eq!(info.info.parent_user.as_deref(), Some("ldap-parent"));
        assert_eq!(info.info.account_status.as_deref(), Some("on"));
        assert_eq!(info.ldap_specific_info.username.as_deref(), Some("alice"));
        assert!(info.open_id_specific_info.is_empty());

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/info-access-key?accessKey=svc-ldap"
        );
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_anonymous_admin_requests_skip_authorization_header() {
        let (endpoint, receiver, handle) =
            start_admin_test_server("200 OK", r#"{"bitrotStartTime":"2026-04-19T10:00:00Z"}"#);
        let client = anonymous_admin_client_for_endpoint(&endpoint);

        client
            .heal_status()
            .await
            .expect("anonymous heal status request");

        let request = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/rustfs/admin/v3/background-heal/status");
        assert!(
            !request
                .headers
                .lines()
                .any(|line| line.to_ascii_lowercase().starts_with("authorization:"))
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_anonymous_admin_no_response_requests_skip_authorization_header() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"clientToken":"token-anon","clientAddress":"","startTime":"2026-06-25T10:00:00Z"}"#,
        );
        let client = anonymous_admin_client_for_endpoint(&endpoint);
        let request = HealStartRequest {
            bucket: Some("raw photos".to_string()),
            prefix: Some("2026/april".to_string()),
            scan_mode: HealScanMode::Deep,
            remove: true,
            recreate: true,
            dry_run: true,
        };

        client
            .heal_start(request)
            .await
            .expect("anonymous heal start request");

        let request = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/heal/raw%20photos/2026%2Fapril"
        );
        assert_heal_options_body(&request.body, false, 2, true, true, true);
        assert!(
            !request
                .headers
                .lines()
                .any(|line| line.to_ascii_lowercase().starts_with("authorization:"))
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_heal_start_posts_to_bucket_prefix_route_with_options_body() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"clientToken":"heal-token-123","clientAddress":"127.0.0.1:9000","startTime":"2026-06-25T10:00:00Z"}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);
        let request = HealStartRequest {
            bucket: Some("raw photos".to_string()),
            prefix: Some("2026/april".to_string()),
            scan_mode: HealScanMode::Deep,
            remove: true,
            recreate: true,
            dry_run: true,
        };

        let status = client
            .heal_start(request)
            .await
            .expect("heal start request");

        assert!(!status.healing);
        assert_eq!(status.heal_id, "heal-token-123");
        assert_eq!(status.bucket, "raw photos");
        assert_eq!(status.object, "2026/april");
        assert_eq!(status.started.as_deref(), Some("2026-06-25T10:00:00Z"));

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/heal/raw%20photos/2026%2Fapril"
        );
        assert_heal_options_body(&request.body, false, 2, true, true, true);
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_heal_start_without_bucket_posts_recursive_root_route() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"clientToken":"root-token","clientAddress":"127.0.0.1:9000","startTime":"2026-06-25T10:00:00Z"}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);
        let request = HealStartRequest {
            scan_mode: HealScanMode::Deep,
            remove: true,
            recreate: true,
            dry_run: true,
            ..Default::default()
        };

        let status = client
            .heal_start(request)
            .await
            .expect("recursive root heal start request");

        assert!(!status.healing);
        assert_eq!(status.heal_id, "root-token");
        assert!(status.bucket.is_empty());
        assert!(status.object.is_empty());
        assert_eq!(status.started.as_deref(), Some("2026-06-25T10:00:00Z"));

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/rustfs/admin/v3/heal/");
        assert_heal_options_body(&request.body, true, 2, true, true, true);
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_heal_task_status_queries_root_route_with_client_token() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"summary":"running","detail":"","startTime":"2026-07-15T00:38:07Z","settings":{"recursive":true,"dryRun":false,"remove":false,"recreate":true,"scanMode":1,"updateParity":false,"nolock":false},"items":[]}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let status = client
            .heal_task_status(HealTaskRequest {
                bucket: String::new(),
                prefix: None,
                client_token: "root-token".to_string(),
            })
            .await
            .expect("root heal task status request");

        assert_eq!(status.heal_id, "root-token");
        assert!(status.healing);
        assert!(status.bucket.is_empty());
        assert!(status.object.is_empty());

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/heal/?clientToken=root-token"
        );
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_heal_task_status_queries_bucket_route_with_client_token() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"summary":"running","detail":"","startTime":"2026-06-25T10:00:00Z","settings":{"recursive":true,"dryRun":false,"remove":false,"recreate":true,"scanMode":2,"updateParity":false,"nolock":false},"items":[],"progress":{"objectsScanned":11,"objectsHealed":5,"objectsFailed":2,"bytesScanned":8192,"bytesHealed":2048}}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let status = client
            .heal_task_status(HealTaskRequest {
                bucket: "raw photos".to_string(),
                prefix: None,
                client_token: "heal-token-123".to_string(),
            })
            .await
            .expect("heal task status request");

        assert_eq!(status.heal_id, "heal-token-123");
        assert!(status.healing);
        assert_eq!(status.bucket, "raw photos");
        assert!(status.object.is_empty());
        assert_eq!(status.scan_mode, Some(HealScanMode::Deep));
        assert_eq!(status.started.as_deref(), Some("2026-06-25T10:00:00Z"));
        assert_eq!(status.items_scanned, 11);
        assert_eq!(status.items_healed, 5);
        assert_eq!(status.items_failed, 2);
        assert_eq!(status.bytes_scanned, 8192);
        assert_eq!(status.bytes_healed, 2048);

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/heal/raw%20photos?clientToken=heal-token-123"
        );
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_heal_task_status_queries_prefix_route_with_client_token() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"summary":"finished","detail":"","startTime":"2026-06-25T10:00:00Z","settings":{"recursive":false,"dryRun":false,"remove":false,"recreate":false,"scanMode":1,"updateParity":false,"nolock":false},"items":[]}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let status = client
            .heal_task_status(HealTaskRequest {
                bucket: "raw photos".to_string(),
                prefix: Some("2026/april".to_string()),
                client_token: "heal-token-123".to_string(),
            })
            .await
            .expect("heal task status request");

        assert_eq!(status.heal_id, "heal-token-123");
        assert!(!status.healing);
        assert_eq!(status.bucket, "raw photos");
        assert_eq!(status.object, "2026/april");
        assert_eq!(status.scan_mode, Some(HealScanMode::Normal));

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/heal/raw%20photos/2026%2Fapril?clientToken=heal-token-123"
        );
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_heal_task_stop_posts_root_force_stop_with_client_token() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"summary":"stopped","detail":"heal task cancelled","startTime":"2026-07-15T00:38:07Z","settings":{"recursive":true,"dryRun":false,"remove":false,"recreate":true,"scanMode":1,"updateParity":false,"nolock":false},"items":[]}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let status = client
            .heal_task_stop(HealTaskRequest {
                bucket: String::new(),
                prefix: None,
                client_token: "root-token".to_string(),
            })
            .await
            .expect("root heal task stop request");

        assert_eq!(status.heal_id, "root-token");
        assert!(!status.healing);
        assert!(status.bucket.is_empty());

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/heal/?clientToken=root-token&forceStop=true"
        );
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_heal_task_stop_posts_force_stop_with_client_token() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"summary":"stopped","detail":"heal task cancelled","startTime":"2026-06-25T10:00:00Z","settings":{"recursive":true,"dryRun":false,"remove":false,"recreate":true,"scanMode":2,"updateParity":false,"nolock":false},"items":[]}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let status = client
            .heal_task_stop(HealTaskRequest {
                bucket: "raw photos".to_string(),
                prefix: None,
                client_token: "heal-token-123".to_string(),
            })
            .await
            .expect("heal task stop request");

        assert_eq!(status.heal_id, "heal-token-123");
        assert!(!status.healing);
        assert_eq!(status.bucket, "raw photos");

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/heal/raw%20photos?clientToken=heal-token-123&forceStop=true"
        );
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_heal_stop_posts_force_stop_to_root_heal_route() {
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", "");
        let client = admin_client_for_endpoint(&endpoint);

        client.heal_stop().await.expect("heal stop request");

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/rustfs/admin/v3/heal/?forceStop=true");
        assert_heal_options_body(&request.body, false, 1, false, false, false);
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_cluster_info_unwraps_beta9_info_response() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"info":{"mode":"distributed","deploymentID":"deployment-123","servers":[{"endpoint":"http://node1:9000","state":"online","drives":[]}]},"admin_discovery":{"runtimeCapabilities":"/rustfs/admin/v4/runtime/capabilities","clusterSnapshot":"/rustfs/admin/v4/cluster/snapshot","extensionsCatalog":"/rustfs/admin/v4/extensions/catalog"}}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let info = client.cluster_info().await.expect("cluster info request");

        assert_eq!(info.mode.as_deref(), Some("distributed"));
        assert_eq!(info.deployment_id.as_deref(), Some("deployment-123"));
        assert_eq!(info.servers.as_ref().map(Vec::len), Some(1));

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/rustfs/admin/v3/info");
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_cluster_info_rejects_flat_beta8_response() {
        let (endpoint, _receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"mode":"distributed","deploymentID":"legacy"}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let error = client
            .cluster_info()
            .await
            .expect_err("flat beta.8 cluster info should be rejected");

        assert!(error.to_string().contains("missing field `info`"));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_pool_status_uses_pool_status_route_with_by_id_query() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"pool":{"id":1,"cmdline":"/data/pool1/disk{1...4}","lastUpdate":"2026-05-06T00:00:00Z","decommissionInfo":null},"admin_discovery":{"runtimeCapabilities":"/rustfs/admin/v4/runtime/capabilities","clusterSnapshot":"/rustfs/admin/v4/cluster/snapshot","extensionsCatalog":"/rustfs/admin/v4/extensions/catalog"}}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let status = client
            .pool_status(PoolTarget {
                pool: "1".to_string(),
                by_id: true,
            })
            .await
            .expect("pool status request");

        assert_eq!(status.id, 1);
        assert_eq!(status.cmd_line, "/data/pool1/disk{1...4}");

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/pools/status?pool=1&by-id=true"
        );
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_pool_status_rejects_flat_beta8_response() {
        let (endpoint, _receiver, handle) =
            start_admin_test_server("200 OK", r#"{"id":1,"cmdline":"/data/pool1/disk{1...4}"}"#);
        let client = admin_client_for_endpoint(&endpoint);

        let error = client
            .pool_status(PoolTarget {
                pool: "1".to_string(),
                by_id: true,
            })
            .await
            .expect_err("flat beta.8 pool status should be rejected");

        assert!(error.to_string().contains("missing field `pool`"));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_pool_status_uses_command_line_query_without_by_id() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"pool":{"id":2,"cmdline":"/data/pool2/disk{1...4}","lastUpdate":"2026-05-06T00:00:00Z","decommissionInfo":null},"admin_discovery":{"runtimeCapabilities":"/rustfs/admin/v4/runtime/capabilities","clusterSnapshot":"/rustfs/admin/v4/cluster/snapshot","extensionsCatalog":"/rustfs/admin/v4/extensions/catalog"}}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let status = client
            .pool_status(PoolTarget {
                pool: "/data/pool2/disk{1...4}".to_string(),
                by_id: false,
            })
            .await
            .expect("pool status request");

        assert_eq!(status.id, 2);
        assert_eq!(status.cmd_line, "/data/pool2/disk{1...4}");

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/pools/status?pool=%2Fdata%2Fpool2%2Fdisk%7B1...4%7D"
        );
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_list_pools_uses_pool_list_route() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"[{"id":0,"cmdline":"/data/pool0/disk{1...4}","lastUpdate":"2026-05-06T00:00:00Z","decommissionInfo":null}]"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let pools = client.list_pools().await.expect("list pools request");

        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].id, 0);
        assert_eq!(pools[0].cmd_line, "/data/pool0/disk{1...4}");

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/rustfs/admin/v3/pools/list");
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_decommission_status_uses_status_route() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"pools":[{"id":0,"cmdline":"/data/pool0/disk{1...4}","status":"running","poolStatus":"decommissioning","decommissionInfo":null}]}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let status = client
            .decommission_status(None)
            .await
            .expect("decommission status request");

        assert_eq!(status.pools.len(), 1);
        assert_eq!(status.pools[0].status, "running");
        assert_eq!(status.pools[0].pool_status, "decommissioning");

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/rustfs/admin/v3/decommission/status");
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_decommission_status_uses_status_route_with_by_id_query() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"id":1,"cmdline":"/data/pool1/disk{1...4}","status":"failed","poolStatus":"blocked","decommissionInfo":null}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let status = client
            .decommission_status(Some(PoolTarget {
                pool: "1".to_string(),
                by_id: true,
            }))
            .await
            .expect("decommission status request");

        assert_eq!(status.pools.len(), 1);
        assert_eq!(status.pools[0].id, 1);
        assert_eq!(status.pools[0].status, "failed");

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/decommission/status?pool=1&by-id=true"
        );
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_decommission_start_posts_pool_query() {
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", "");
        let client = admin_client_for_endpoint(&endpoint);

        client
            .decommission_start(PoolTarget {
                pool: "/data/pool1/disk{1...4}".to_string(),
                by_id: false,
            })
            .await
            .expect("decommission start request");

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/pools/decommission?pool=%2Fdata%2Fpool1%2Fdisk%7B1...4%7D"
        );
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_decommission_start_posts_by_id_multi_pool_query() {
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", "");
        let client = admin_client_for_endpoint(&endpoint);

        client
            .decommission_start(PoolTarget {
                pool: "1,2".to_string(),
                by_id: true,
            })
            .await
            .expect("decommission start request");

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/pools/decommission?pool=1%2C2&by-id=true"
        );
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_decommission_cancel_posts_pool_cancel_route_with_by_id_query() {
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", "");
        let client = admin_client_for_endpoint(&endpoint);

        client
            .decommission_cancel(PoolTarget {
                pool: "1".to_string(),
                by_id: true,
            })
            .await
            .expect("decommission cancel request");

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/pools/cancel?pool=1&by-id=true"
        );
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_decommission_clear_posts_pool_clear_route_with_by_id_query() {
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", "");
        let client = admin_client_for_endpoint(&endpoint);

        client
            .decommission_clear(PoolTarget {
                pool: "3".to_string(),
                by_id: true,
            })
            .await
            .expect("decommission clear request");

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/pools/clear?pool=3&by-id=true"
        );
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_rebalance_start_posts_rebalance_start_route() {
        let (endpoint, receiver, handle) =
            start_admin_test_server("200 OK", r#"{"id":"rebalance-123"}"#);
        let client = admin_client_for_endpoint(&endpoint);

        let result = client
            .rebalance_start()
            .await
            .expect("rebalance start request");

        assert_eq!(result.id, "rebalance-123");

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/rustfs/admin/v3/rebalance/start");
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_rebalance_status_gets_rebalance_status_route() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"id":"rebalance-123","pools":[],"stoppedAt":"2026-05-06T00:00:00Z"}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let status = client
            .rebalance_status()
            .await
            .expect("rebalance status request");

        assert_eq!(status.id, "rebalance-123");
        assert_eq!(status.stopped_at.as_deref(), Some("2026-05-06T00:00:00Z"));

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/rustfs/admin/v3/rebalance/status");
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_rebalance_stop_posts_rebalance_stop_route() {
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", "");
        let client = admin_client_for_endpoint(&endpoint);

        client
            .rebalance_stop()
            .await
            .expect("rebalance stop request");

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/rustfs/admin/v3/rebalance/stop");
        assert!(request.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_site_replication_add_puts_peer_sites_body() {
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", r#"{"success":true}"#);
        let client = admin_client_for_endpoint(&endpoint);

        let sites = vec![PeerSiteSpec {
            name: "site-a".to_string(),
            endpoint: "https://site-a.example:9000".to_string(),
            access_key: "site-a-access".to_string(),
            secret_key: "site-a-secret".to_string(),
            skip_tls_verify: true,
            ca_cert_pem: String::new(),
        }];

        let result = client
            .site_replication_add(&sites)
            .await
            .expect("site replication add request");

        assert_eq!(result["success"], true);
        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "PUT");
        assert_eq!(request.target, "/rustfs/admin/v3/site-replication/add");

        let body: Vec<PeerSiteSpec> =
            serde_json::from_slice(&request.body).expect("peer sites body should be JSON");
        assert_eq!(body.len(), 1);
        assert_eq!(body[0].name, "site-a");
        assert_eq!(body[0].endpoint, "https://site-a.example:9000");
        assert_eq!(body[0].access_key, "site-a-access");
        assert_eq!(body[0].secret_key, "site-a-secret");
        assert!(body[0].skip_tls_verify);
        handle.join().expect("server thread should finish");
    }

    const SITE_REPLICATION_INFO_RESPONSE: &str = r#"{
        "enabled":true,
        "name":"primary",
        "sites":[{
            "endpoint":"https://secondary.example.test",
            "name":"secondary",
            "deploymentID":"deployment-2",
            "sync":"enable",
            "defaultbandwidth":{"bandwidthLimitPerBucket":1024,"set":true},
            "replicate-ilm-expiry":true,
            "objectNamingMode":"path",
            "skipTlsVerify":false,
            "caCertPem":"ORIGINAL-CA",
            "apiVersion":"v1",
            "futurePeer":{"mode":"preserved","accessToken":"OPAQUE-TOKEN-MUST-NOT-PRINT"}
        }],
        "serviceAccountAccessKey":"DISCARDED",
        "apiVersion":"v1"
    }"#;

    #[tokio::test]
    async fn site_replication_info_returns_typed_opaque_snapshot() {
        let (endpoint, receiver, handle) =
            start_admin_test_server("200 OK", SITE_REPLICATION_INFO_RESPONSE);
        let client = admin_client_for_endpoint(&endpoint);

        let info = client
            .site_replication_info()
            .await
            .expect("site replication info request");

        assert_eq!(info.sites[0].deployment_id(), Some("deployment-2"));
        assert_eq!(info.sites[0].ca_cert_pem(), Some("ORIGINAL-CA"));
        let wire = serde_json::to_value(&info.sites[0]).expect("peer is serializable");
        assert_eq!(
            wire["futurePeer"]["accessToken"],
            "OPAQUE-TOKEN-MUST-NOT-PRINT"
        );
        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/rustfs/admin/v3/site-replication/info");
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_edit_puts_lossless_peer_snapshot() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"success":true,"status":"updated","errorDetail":"","apiVersion":"v1"}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);
        let info: rc_core::admin::SiteReplicationInfo =
            serde_json::from_str(SITE_REPLICATION_INFO_RESPONSE).expect("valid info fixture");

        let status = client
            .site_replication_edit(&info.sites[0])
            .await
            .expect("site replication edit request");

        assert!(status.success);
        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "PUT");
        assert_eq!(request.target, "/rustfs/admin/v3/site-replication/edit");
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).expect("edit body should be JSON");
        assert_eq!(body["caCertPem"], "ORIGINAL-CA");
        assert_eq!(body["futurePeer"]["mode"], "preserved");
        assert_eq!(
            body["futurePeer"]["accessToken"],
            "OPAQUE-TOKEN-MUST-NOT-PRINT"
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_info_rejects_declared_success_overflow() {
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n",
            rc_core::admin::MAX_SITE_REPLICATION_SUCCESS_RESPONSE_BYTES + 1
        )
        .into_bytes();
        let (endpoint, handle) = start_admin_raw_response_server(response);
        let client = admin_client_for_endpoint(&endpoint);

        let error = client
            .site_replication_info()
            .await
            .expect_err("oversized declared response must fail");

        assert!(matches!(error, Error::General(_)));
        assert!(error.to_string().contains("exceeds"));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_info_rejects_chunked_success_overflow() {
        let mut response = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n".to_vec();
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..=rc_core::admin::MAX_SITE_REPLICATION_SUCCESS_RESPONSE_BYTES / chunk.len() {
            response.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            response.extend_from_slice(&chunk);
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"0\r\n\r\n");
        let (endpoint, handle) = start_admin_raw_response_server(response);
        let client = admin_client_for_endpoint(&endpoint);

        let error = client
            .site_replication_info()
            .await
            .expect_err("oversized chunked response must fail");

        assert!(matches!(error, Error::General(_)));
        assert!(error.to_string().contains("exceeds"));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_edit_rejects_chunked_error_overflow() {
        let mut response = b"HTTP/1.1 500 Internal Server Error\r\ntransfer-encoding: chunked\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n".to_vec();
        let body = vec![b'x'; rc_core::admin::MAX_SITE_REPLICATION_ERROR_RESPONSE_BYTES + 1];
        response.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
        response.extend_from_slice(&body);
        response.extend_from_slice(b"\r\n0\r\n\r\n");
        let (endpoint, handle) = start_admin_raw_response_server(response);
        let client = admin_client_for_endpoint(&endpoint);
        let peer = rc_core::admin::SiteReplicationPeer::default();

        let error = client
            .site_replication_edit(&peer)
            .await
            .expect_err("oversized chunked error must fail");

        assert!(matches!(error, Error::General(_)));
        assert!(error.to_string().contains("exceeds"));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_info_rejects_malformed_json() {
        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", "{");
        let client = admin_client_for_endpoint(&endpoint);

        let error = client
            .site_replication_info()
            .await
            .expect_err("malformed JSON must fail");

        assert!(matches!(error, Error::Json(_)));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_edit_rejects_oversized_request_before_network() {
        let client = admin_client_for_endpoint("http://127.0.0.1:1");
        let mut peer = rc_core::admin::SiteReplicationPeer::default();
        peer.set_ca_cert_pem("x".repeat(rc_core::admin::MAX_SITE_REPLICATION_REQUEST_BYTES + 1));

        let error = client
            .site_replication_edit(&peer)
            .await
            .expect_err("oversized request must fail before connecting");

        assert!(matches!(error, Error::RequestRejected(_)));
        assert!(error.to_string().contains("exceeds"));
    }

    #[tokio::test]
    async fn site_replication_edit_treats_success_false_as_failure() {
        let (endpoint, _receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"success":false,"status":"rejected","errorDetail":"peer is invalid","apiVersion":"v1"}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);
        let peer = rc_core::admin::SiteReplicationPeer::default();

        let error = client
            .site_replication_edit(&peer)
            .await
            .expect_err("success false must fail");

        assert!(matches!(error, Error::General(_)));
        assert!(!error.to_string().contains("peer is invalid"));
        assert!(error.to_string().contains("rejected by the server"));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn kms_create_key_posts_name_as_reserved_tag_and_ignores_key_material() {
        let body = r#"{"success":true,"message":"created","key_id":"key-123","key_metadata":{"key_id":"key-123","key_state":"Enabled","key_usage":"EncryptDecrypt","tags":{"name":"archive"},"plaintext_key":"MUST_NOT_APPEAR"},"plaintext_key":"MUST_NOT_APPEAR"}"#;
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", body);
        let client = admin_client_for_endpoint(&endpoint);
        let request = KmsCreateKeyRequest {
            name: Some("archive".to_string()),
            description: Some("Archive key".to_string()),
            tags: BTreeMap::from([("environment".to_string(), "prod".to_string())]),
        };

        let result = client
            .kms_create_key(&request)
            .await
            .expect("KMS key should be created");

        assert_eq!(result.key_id, "key-123");
        assert_eq!(
            result
                .key
                .as_ref()
                .and_then(|key| key.tags.get("name"))
                .map(String::as_str),
            Some("archive")
        );
        assert!(!format!("{result:?}").contains("MUST_NOT_APPEAR"));
        let captured = receiver.recv().expect("captured request");
        assert_eq!(captured.method, "POST");
        assert_eq!(captured.target, "/rustfs/admin/v3/kms/keys");
        let request_body: serde_json::Value =
            serde_json::from_slice(&captured.body).expect("create request should be JSON");
        assert_eq!(request_body["description"], "Archive key");
        assert_eq!(request_body["tags"]["name"], "archive");
        assert_eq!(request_body["tags"]["environment"], "prod");
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn kms_delete_and_cancel_use_native_lifecycle_routes() {
        let responses = vec![
            (
                "200 OK",
                r#"{"success":true,"message":"scheduled","key_id":"key-123","deletion_date":"2026-07-28T00:00:00Z"}"#,
            ),
            (
                "200 OK",
                r#"{"success":true,"message":"cancelled","key_id":"key-123","key_metadata":{"key_id":"key-123","key_state":"Enabled","key_usage":"EncryptDecrypt","tags":{}}}"#,
            ),
        ];
        let (endpoint, receiver, handle) = start_admin_sequence_server(responses);
        let client = admin_client_for_endpoint(&endpoint);

        let deleted = client
            .kms_delete_key(&KmsDeleteKeyRequest {
                key_id: "key-123".to_string(),
                pending_window_in_days: Some(7),
                force_immediate: false,
            })
            .await
            .expect("KMS deletion should be scheduled");
        assert_eq!(
            deleted.deletion_date.as_deref(),
            Some("2026-07-28T00:00:00Z")
        );
        assert!(!deleted.immediate);

        let cancelled = client
            .kms_cancel_key_deletion("key-123")
            .await
            .expect("KMS deletion should be cancelled");
        assert_eq!(cancelled.key_id, "key-123");

        let delete_request = receiver.recv().expect("captured delete request");
        assert_eq!(delete_request.method, "DELETE");
        assert_eq!(delete_request.target, "/rustfs/admin/v3/kms/keys/delete");
        let delete_body: serde_json::Value =
            serde_json::from_slice(&delete_request.body).expect("delete request should be JSON");
        assert_eq!(delete_body["pending_window_in_days"], 7);
        assert!(delete_body.get("force_immediate").is_none());

        let cancel_request = receiver.recv().expect("captured cancel request");
        assert_eq!(cancel_request.method, "POST");
        assert_eq!(
            cancel_request.target,
            "/rustfs/admin/v3/kms/keys/cancel-deletion"
        );
        let cancel_body: serde_json::Value =
            serde_json::from_slice(&cancel_request.body).expect("cancel request should be JSON");
        assert_eq!(cancel_body["key_id"], "key-123");
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn kms_mutation_errors_are_typed_and_do_not_echo_server_details() {
        for (status, body, expected) in [
            (
                "200 OK",
                r#"{"success":false,"message":"key already exists: MUST_NOT_APPEAR"}"#,
                "conflict",
            ),
            (
                "500 Internal Server Error",
                r#"{"success":false,"message":"key already exists: MUST_NOT_APPEAR"}"#,
                "conflict",
            ),
            (
                "200 OK",
                r#"{"success":false,"message":"kms service is not running: MUST_NOT_APPEAR"}"#,
                "network",
            ),
            (
                "503 Service Unavailable",
                r#"{"success":false,"message":"kms service is not running: MUST_NOT_APPEAR"}"#,
                "network",
            ),
        ] {
            let (endpoint, _receiver, handle) = start_admin_test_server(status, body);
            let client = admin_client_for_endpoint(&endpoint);
            let error = client
                .kms_create_key(&KmsCreateKeyRequest {
                    name: None,
                    description: None,
                    tags: BTreeMap::new(),
                })
                .await
                .expect_err("mutation should fail");
            match expected {
                "conflict" => assert!(matches!(error, Error::Conflict(_))),
                "network" => assert!(matches!(error, Error::Network(_))),
                _ => panic!("unexpected expected error kind"),
            }
            assert!(!error.to_string().contains("MUST_NOT_APPEAR"));
            handle.join().expect("server thread should finish");
        }
    }

    #[tokio::test]
    async fn kms_configure_posts_strict_sensitive_request_without_debug_exposure() {
        let body = r#"{"success":true,"message":"configured","status":"Configured"}"#;
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", body);
        let client = admin_client_for_endpoint(&endpoint);
        let request: KmsConfigureRequest = serde_json::from_str(
            r#"{"backend_type":"VaultKV2","address":"https://vault.example","auth_method":{"Token":{"token":"VAULT_TOKEN_MUST_NOT_APPEAR"}},"mount_path":"transit","kv_mount":"secret","key_path_prefix":"rustfs/kms/keys"}"#,
        )
        .expect("configuration should deserialize");

        let state = client
            .kms_configure(&request)
            .await
            .expect("KMS should configure");

        assert_eq!(state, KmsServiceState::Configured);
        let captured = receiver.recv().expect("captured configure request");
        assert_eq!(captured.method, "POST");
        assert_eq!(captured.target, "/rustfs/admin/v3/kms/configure");
        let request_body: serde_json::Value =
            serde_json::from_slice(&captured.body).expect("configure request should be JSON");
        assert_eq!(request_body["backend_type"], "VaultKV2");
        assert_eq!(
            request_body["auth_method"]["Token"]["token"],
            "VAULT_TOKEN_MUST_NOT_APPEAR"
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn kms_reconfigure_start_restart_and_stop_use_native_routes() {
        let responses = vec![
            (
                "200 OK",
                r#"{"success":true,"message":"reconfigured","status":"Running"}"#,
            ),
            (
                "200 OK",
                r#"{"success":true,"message":"started","status":"Running"}"#,
            ),
            (
                "200 OK",
                r#"{"success":true,"message":"restarted","status":"Running"}"#,
            ),
            (
                "200 OK",
                r#"{"success":true,"message":"stopped","status":"Configured"}"#,
            ),
        ];
        let (endpoint, receiver, handle) = start_admin_sequence_server(responses);
        let client = admin_client_for_endpoint(&endpoint);
        let request: KmsConfigureRequest = serde_json::from_str(
            r#"{"backend_type":"VaultTransit","address":"https://vault.example","auth_method":{"Token":{"token":""}},"mount_path":"transit"}"#,
        )
        .expect("configuration should deserialize");

        assert_eq!(
            client
                .kms_reconfigure(&request)
                .await
                .expect("KMS should reconfigure"),
            KmsServiceState::Running
        );
        assert_eq!(
            client.kms_start(false).await.expect("KMS should start"),
            KmsServiceState::Running
        );
        assert_eq!(
            client.kms_start(true).await.expect("KMS should restart"),
            KmsServiceState::Running
        );
        assert_eq!(
            client.kms_stop().await.expect("KMS should stop"),
            KmsServiceState::Configured
        );

        let reconfigure = receiver.recv().expect("captured reconfigure request");
        assert_eq!(reconfigure.target, "/rustfs/admin/v3/kms/reconfigure");
        let start = receiver.recv().expect("captured start request");
        assert_eq!(start.target, "/rustfs/admin/v3/kms/start");
        let start_body: serde_json::Value =
            serde_json::from_slice(&start.body).expect("start body should be JSON");
        assert_eq!(start_body["force"], false);
        let restart = receiver.recv().expect("captured restart request");
        let restart_body: serde_json::Value =
            serde_json::from_slice(&restart.body).expect("restart body should be JSON");
        assert_eq!(restart_body["force"], true);
        let stop = receiver.recv().expect("captured stop request");
        assert_eq!(stop.target, "/rustfs/admin/v3/kms/stop");
        assert!(stop.body.is_empty());
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn kms_lifecycle_errors_are_typed_and_redacted() {
        for (status, body, expected) in [
            (
                "200 OK",
                r#"{"success":false,"message":"KMS service is not configured: MUST_NOT_APPEAR","status":"NotConfigured"}"#,
                "not_found",
            ),
            (
                "403 Forbidden",
                r#"{"message":"denied token MUST_NOT_APPEAR"}"#,
                "auth",
            ),
            (
                "503 Service Unavailable",
                r#"{"message":"unavailable token MUST_NOT_APPEAR"}"#,
                "network",
            ),
        ] {
            let (endpoint, _receiver, handle) = start_admin_test_server(status, body);
            let client = admin_client_for_endpoint(&endpoint);
            let error = client
                .kms_start(false)
                .await
                .expect_err("start should fail");
            match expected {
                "not_found" => assert!(matches!(error, Error::NotFound(_))),
                "auth" => assert!(matches!(error, Error::Auth(_))),
                "network" => assert!(matches!(error, Error::Network(_))),
                _ => panic!("unexpected error kind"),
            }
            assert!(!error.to_string().contains("MUST_NOT_APPEAR"));
            handle.join().expect("server thread should finish");
        }
    }

    #[tokio::test]
    async fn site_replication_edit_maps_typed_state_changes_to_conflict() {
        for message in [
            "site replication state changed",
            "site replication refresh state changed",
        ] {
            let body = format!(
                r#"{{"success":false,"status":"{message}","errorDetail":"","apiVersion":"v1"}}"#
            );
            let leaked: &'static str = Box::leak(body.into_boxed_str());
            let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", leaked);
            let client = admin_client_for_endpoint(&endpoint);
            let peer = rc_core::admin::SiteReplicationPeer::default();

            let error = client
                .site_replication_edit(&peer)
                .await
                .expect_err("typed state change must conflict");

            assert!(matches!(error, Error::Conflict(_)));
            assert_eq!(error.exit_code(), 6);
            handle.join().expect("server thread should finish");
        }
    }

    #[tokio::test]
    async fn site_replication_edit_maps_typed_known_pending_phrases_to_conflict() {
        for message in [
            "site replication operation pending",
            "site replication peer edit pending",
            "site replication IAM change pending",
        ] {
            let body = format!(
                r#"{{"success":false,"status":"{message}","errorDetail":"","apiVersion":"v1"}}"#
            );
            let leaked: &'static str = Box::leak(body.into_boxed_str());
            let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", leaked);
            let client = admin_client_for_endpoint(&endpoint);
            let peer = rc_core::admin::SiteReplicationPeer::default();

            let error = client
                .site_replication_edit(&peer)
                .await
                .expect_err("known typed pending state must conflict");

            assert!(matches!(error, Error::Conflict(_)));
            assert_eq!(error.exit_code(), 6);
            handle.join().expect("server thread should finish");
        }
    }

    #[tokio::test]
    async fn site_replication_edit_does_not_overclassify_arbitrary_pending_text() {
        let typed_body = r#"{"success":false,"status":"pending value is malformed","errorDetail":"","apiVersion":"v1"}"#;
        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", typed_body);
        let client = admin_client_for_endpoint(&endpoint);
        let peer = rc_core::admin::SiteReplicationPeer::default();

        let typed_error = client
            .site_replication_edit(&peer)
            .await
            .expect_err("arbitrary typed pending text must fail");

        assert!(matches!(typed_error, Error::General(_)));
        assert_eq!(typed_error.exit_code(), 1);
        handle.join().expect("server thread should finish");

        let http_body = r#"{"code":"InvalidRequest","message":"pending value is malformed"}"#;
        let (endpoint, _receiver, handle) = start_admin_test_server("400 Bad Request", http_body);
        let client = admin_client_for_endpoint(&endpoint);

        let http_error = client
            .site_replication_edit(&peer)
            .await
            .expect_err("arbitrary HTTP pending text must fail");

        assert!(matches!(http_error, Error::General(_)));
        assert_eq!(http_error.exit_code(), 1);
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_edit_does_not_follow_temporary_redirect() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "307 Temporary Redirect",
            r#"{"message":"do not redirect mutations"}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);
        let peer = rc_core::admin::SiteReplicationPeer::default();

        let error = client
            .site_replication_edit(&peer)
            .await
            .expect_err("redirect must not be followed");

        assert!(matches!(error, Error::Network(_)));
        let request = receiver.recv().expect("single captured request");
        assert_eq!(request.method, "PUT");
        assert_eq!(request.target, "/rustfs/admin/v3/site-replication/edit");
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_edit_disconnect_reports_unknown_outcome_without_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            sender
                .send(read_admin_request(&mut stream))
                .expect("send captured request");
        });
        let client = admin_client_for_endpoint(&endpoint);
        let peer = rc_core::admin::SiteReplicationPeer::default();

        let error = client
            .site_replication_edit(&peer)
            .await
            .expect_err("disconnect after PUT must be an unknown outcome");

        assert!(matches!(error, Error::Network(_)));
        assert_eq!(error.exit_code(), 3);
        assert!(error.to_string().contains("outcome is unknown"));
        assert!(error.to_string().contains("not retried"));
        let request = receiver.recv().expect("single captured request");
        assert_eq!(request.method, "PUT");
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_edit_maps_pending_bad_request_to_conflict() {
        let (endpoint, _receiver, handle) = start_admin_test_server(
            "400 Bad Request",
            r#"{"code":"SiteReplicationOperationPending","message":"another site replication operation is pending"}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);
        let peer = rc_core::admin::SiteReplicationPeer::default();

        let error = client
            .site_replication_edit(&peer)
            .await
            .expect_err("pending operation must conflict");

        assert!(matches!(error, Error::Conflict(_)));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_edit_maps_beta10_state_changes_to_conflict() {
        for message in [
            "site replication state changed",
            "site replication refresh state changed",
        ] {
            let body = format!(r#"{{"code":"InvalidRequest","message":"{message}"}}"#);
            let leaked: &'static str = Box::leak(body.into_boxed_str());
            let (endpoint, _receiver, handle) = start_admin_test_server("400 Bad Request", leaked);
            let client = admin_client_for_endpoint(&endpoint);
            let peer = rc_core::admin::SiteReplicationPeer::default();

            let error = client
                .site_replication_edit(&peer)
                .await
                .expect_err("beta10 state change must conflict");

            assert!(matches!(error, Error::Conflict(_)));
            assert_eq!(error.exit_code(), 6);
            handle.join().expect("server thread should finish");
        }
    }

    #[tokio::test]
    async fn site_replication_info_unsupported_message_names_info_operation() {
        let (endpoint, _receiver, handle) = start_admin_test_server("404 Not Found", "");
        let client = admin_client_for_endpoint(&endpoint);

        let error = client
            .site_replication_info()
            .await
            .expect_err("missing info route must be unsupported");
        let message = error.to_string();

        assert!(matches!(error, Error::UnsupportedFeature(_)));
        assert!(message.contains("Site replication info"));
        assert!(!message.contains("edit"));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_edit_rejects_malformed_success_json() {
        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", "{");
        let client = admin_client_for_endpoint(&endpoint);
        let peer = rc_core::admin::SiteReplicationPeer::default();

        let error = client
            .site_replication_edit(&peer)
            .await
            .expect_err("malformed JSON must fail");

        assert!(matches!(error, Error::Json(_)));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_edit_maps_method_not_allowed_to_unsupported() {
        let (endpoint, _receiver, handle) = start_admin_test_server("405 Method Not Allowed", "");
        let client = admin_client_for_endpoint(&endpoint);
        let peer = rc_core::admin::SiteReplicationPeer::default();

        let error = client
            .site_replication_edit(&peer)
            .await
            .expect_err("missing edit route must be unsupported");

        assert!(matches!(error, Error::UnsupportedFeature(_)));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn scanner_status_uses_beta10_route_and_typed_response() {
        let body = r#"{
            "enabled":true,
            "disabled_reason":null,
            "freshness":{"state":"fresh","last_cycle_end_unix_secs":10,"max_expected_age_seconds":120,"reason":null},
            "metrics":{"collected_at":"2026-07-21T04:00:00Z","current_cycle":7,"last_cycle_end_unix_secs":10,"last_cycle_result":"success"},
            "cycle_schedule":{"effective_interval_seconds":60,"clean_idle_backoff_enabled":false,"clean_idle_backoff_multiplier":1},
            "runtime_config":{"speed":{"value":"fast","source":"default"}}
        }"#;
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", body);
        let client = admin_client_for_endpoint(&endpoint);

        let status = client
            .scanner_status()
            .await
            .expect("scanner status should succeed");

        assert_eq!(status.health(), rc_core::admin::ScannerHealth::Healthy);
        assert_eq!(status.metrics.current_cycle, 7);
        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/rustfs/admin/v3/scanner/status");
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_error_does_not_echo_sensitive_server_body() {
        let (endpoint, _receiver, handle) = start_admin_test_server(
            "409 Conflict",
            r#"{"code":"Conflict","message":"accessKey=DO-NOT-ECHO secret=DO-NOT-ECHO"}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);
        let peer = rc_core::admin::SiteReplicationPeer::default();

        let error = client
            .site_replication_edit(&peer)
            .await
            .expect_err("conflict must fail");
        let message = error.to_string();

        assert!(matches!(error, Error::Conflict(_)));
        assert!(!message.contains("DO-NOT-ECHO"));
        assert!(!message.contains("accessKey"));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn storage_info_unwraps_current_response_envelope() {
        let body = r#"{
            "info":{
                "disks":[{"endpoint":"node1","path":"/data1","state":"online","totalspace":100,"usedspace":40,"availspace":60}],
                "backend":{"BackendType":"Erasure","OnlineDisks":{"set-1":1},"OfflineDisks":{}}
            },
            "admin_discovery":{}
        }"#;
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", body);
        let client = admin_client_for_endpoint(&endpoint);

        let info = client
            .storage_info()
            .await
            .expect("storage info should succeed");

        assert_eq!(info.disks.len(), 1);
        assert_eq!(info.total_capacity(), 100);
        let request = receiver.recv().expect("captured request");
        assert_eq!(request.target, "/rustfs/admin/v3/storageinfo");
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_edit_redacts_alias_credentials_from_typed_status() {
        let (endpoint, _receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"success":true,"status":"updated by access using secret","errorDetail":"access secret","apiVersion":"v1"}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);
        let peer = rc_core::admin::SiteReplicationPeer::default();

        let status = client
            .site_replication_edit(&peer)
            .await
            .expect("edit status request");

        assert!(!status.status.contains("access"));
        assert!(!status.status.contains("secret"));
        assert!(!status.error_detail.contains("access"));
        assert!(!status.error_detail.contains("secret"));
        assert!(status.status.contains("[REDACTED]"));
        handle.join().expect("server thread should finish");
    }

    fn site_replication_resync_peer() -> SiteReplicationPeer {
        serde_json::from_str(
            r#"{
                "endpoint":"https://secondary.example.test",
                "name":"secondary",
                "deploymentID":"deployment-2",
                "sync":"enable",
                "futurePeer":{"mode":"preserved"}
            }"#,
        )
        .expect("valid resync peer")
    }

    const SITE_REPLICATION_RESYNC_RESPONSE: &str = r#"{
        "op":"start",
        "id":"resync-123",
        "status":"success",
        "buckets":[{"bucket":"photos","status":"started","errorDetail":""}],
        "errorDetail":"",
        "generation":7
    }"#;

    #[tokio::test]
    async fn site_replication_resync_sends_exact_start_query_and_complete_peer() {
        let (endpoint, receiver, handle) =
            start_admin_test_server("200 OK", SITE_REPLICATION_RESYNC_RESPONSE);
        let client = admin_client_for_endpoint(&endpoint);
        let peer = site_replication_resync_peer();

        let status = client
            .site_replication_resync(SiteReplicationResyncOperation::Start, &peer)
            .await
            .expect("resync start request");

        let returned = serde_json::to_value(status).expect("resync status serializes");
        assert_eq!(returned["op"], "start");
        assert_eq!(returned["id"], "resync-123");
        assert_eq!(returned["buckets"][0]["bucket"], "photos");
        assert_eq!(returned["generation"], 7);

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "PUT");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/site-replication/resync/op?operation=start"
        );
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).expect("resync body should be JSON");
        assert_eq!(body["endpoint"], "https://secondary.example.test");
        assert_eq!(body["deploymentID"], "deployment-2");
        assert_eq!(body["sync"], "enable");
        assert_eq!(body["futurePeer"]["mode"], "preserved");
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_resync_status_works_with_a_fresh_client() {
        let (endpoint, receiver, handle) = start_admin_sequence_server(vec![
            ("200 OK", SITE_REPLICATION_RESYNC_RESPONSE),
            ("200 OK", SITE_REPLICATION_RESYNC_RESPONSE),
        ]);
        let peer = site_replication_resync_peer();
        let first_client = admin_client_for_endpoint(&endpoint);
        first_client
            .site_replication_resync(SiteReplicationResyncOperation::Start, &peer)
            .await
            .expect("resync start request");
        drop(first_client);

        let fresh_client = admin_client_for_endpoint(&endpoint);
        let status = fresh_client
            .site_replication_resync(SiteReplicationResyncOperation::Status, &peer)
            .await
            .expect("fresh-client resync status request");

        assert_eq!(
            serde_json::to_value(status).expect("status serializes")["id"],
            "resync-123"
        );
        let start = receiver.recv().expect("captured start request");
        let status = receiver.recv().expect("captured status request");
        assert_eq!(
            start.target,
            "/rustfs/admin/v3/site-replication/resync/op?operation=start"
        );
        assert_eq!(
            status.target,
            "/rustfs/admin/v3/site-replication/resync/op?operation=status"
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_resync_redacts_known_and_nested_extension_values() {
        let response = r#"{
            "op":"start-access",
            "id":"secret-id",
            "status":"access secret",
            "buckets":[{
                "bucket":"access-bucket",
                "status":"secret-status",
                "errorDetail":"access secret",
                "futureBucket":"access secret"
            }],
            "errorDetail":"access secret",
            "futureTop":{
                "updatedAt":"access secret",
                "progress":["access",{"message":"secret"}]
            }
        }"#;
        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", response);
        let client = admin_client_for_endpoint(&endpoint);

        let status = client
            .site_replication_resync(
                SiteReplicationResyncOperation::Status,
                &site_replication_resync_peer(),
            )
            .await
            .expect("resync status request");
        let value = serde_json::to_value(status).expect("resync status serializes");

        for field in [
            &value["op"],
            &value["id"],
            &value["status"],
            &value["errorDetail"],
            &value["buckets"][0]["bucket"],
            &value["buckets"][0]["status"],
            &value["buckets"][0]["errorDetail"],
        ] {
            let field = field.as_str().expect("known string field");
            assert!(!field.contains("access"));
            assert!(!field.contains("secret"));
            assert!(field.contains("[REDACTED]"));
        }
        assert_eq!(value["futureTop"]["updatedAt"], "[REDACTED] [REDACTED]");
        assert_eq!(value["futureTop"]["progress"][0], "[REDACTED]");
        assert_eq!(value["futureTop"]["progress"][1]["message"], "[REDACTED]");
        assert_eq!(value["buckets"][0]["futureBucket"], "[REDACTED] [REDACTED]");
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn realtime_metrics_encodes_exact_query_and_reads_ndjson_incrementally() {
        let body = concat!(
            "{\"errors\":[],\"hosts\":[],\"aggregated\":{\"scanner\":{\"collected\":\"2026-07-21T04:00:00Z\",\"current_cycle\":7}},\"by_host\":{},\"by_disk\":{},\"final\":false}\n",
            "{\"errors\":[],\"hosts\":[],\"aggregated\":{\"scanner\":{\"collected\":\"2026-07-21T04:00:03Z\",\"current_cycle\":8}},\"by_host\":{},\"by_disk\":{},\"final\":true}\n"
        );
        let (endpoint, receiver, handle) =
            start_admin_owned_test_server("200 OK", "application/x-ndjson", body.to_string());
        let client = admin_client_for_endpoint(&endpoint);
        let query = rc_core::admin::MetricsQuery {
            scopes: vec![
                rc_core::admin::MetricsScope::Scanner,
                rc_core::admin::MetricsScope::Disk,
            ],
            hosts: vec!["node 1".to_string()],
            disks: vec!["/data/one".to_string()],
            interval: Some("3s".to_string()),
            samples: 2,
            by_host: true,
            by_disk: true,
            job_id: Some("job/1".to_string()),
            deployment_id: Some("dep 1".to_string()),
        };

        let batch = client
            .realtime_metrics(&query)
            .await
            .expect("metrics should succeed");

        assert_eq!(batch.snapshots.len(), 2);
        assert!(!batch.is_partial());
        assert!(batch.encoded_bytes > 0);
        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/metrics?disks=%2Fdata%2Fone&hosts=node%201&interval=3s&n=2&types=3&by-disk=true&by-host=true&by-jobID=job%2F1&by-depID=dep%201"
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_resync_preserves_semantics_when_credentials_match_status_values() {
        let response = r#"{
            "op":"start",
            "id":"resync-1",
            "status":"success",
            "buckets":[{"bucket":"photos","status":"started"}]
        }"#;
        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", response);
        let alias = Alias::new("test", &endpoint, "success", "success-extra");
        let client = AdminClient::new(&alias).expect("admin client should build");

        let status = client
            .site_replication_resync(
                SiteReplicationResyncOperation::Start,
                &site_replication_resync_peer(),
            )
            .await
            .expect("resync start request");

        assert!(!status.has_failure());
        assert_eq!(status.status, "[REDACTED]");
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_resync_redacts_overlapping_credentials_longest_first() {
        let response = r#"{
            "op":"start",
            "id":"resync-1",
            "status":"success",
            "updatedAt":"ABCDEF ABC"
        }"#;
        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", response);
        let alias = Alias::new("test", &endpoint, "ABC", "ABCDEF");
        let client = AdminClient::new(&alias).expect("admin client should build");

        let status = client
            .site_replication_resync(
                SiteReplicationResyncOperation::Start,
                &site_replication_resync_peer(),
            )
            .await
            .expect("resync start request");
        let value = serde_json::to_value(status).expect("resync status serializes");

        assert_eq!(value["updatedAt"], "[REDACTED] [REDACTED]");
        assert!(!value.to_string().contains("DEF"));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_resync_rejects_incomplete_success_payloads_without_echoing_them() {
        for malformed in [
            r#"{"status":"success","id":"resync-1","marker":"DO-NOT-ECHO"}"#,
            r#"{"op":"start","id":"resync-1","marker":"DO-NOT-ECHO"}"#,
            r#"{"op":"start","status":"success","marker":"DO-NOT-ECHO"}"#,
            r#"{"op":"cancel","status":"success","marker":"DO-NOT-ECHO"}"#,
            r#"{"op":"start","status":"not-found","marker":"DO-NOT-ECHO"}"#,
            r#"{"op":"start","id":"resync-1","status":"success","buckets":[{"status":"started"}],"marker":"DO-NOT-ECHO"}"#,
            r#"{"op":"start","id":"resync-1","status":"success","buckets":[{"bucket":"photos"}],"marker":"DO-NOT-ECHO"}"#,
        ] {
            let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", malformed);
            let client = admin_client_for_endpoint(&endpoint);
            let error = client
                .site_replication_resync(
                    SiteReplicationResyncOperation::Status,
                    &site_replication_resync_peer(),
                )
                .await
                .expect_err("incomplete resync response must fail");

            assert!(matches!(error, Error::General(_)));
            assert_eq!(error.exit_code(), 1);
            assert!(!error.to_string().contains("DO-NOT-ECHO"));
            handle.join().expect("server thread should finish");
        }

        let (endpoint, _receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"op":"status","status":"success","marker":"DO-NOT-ECHO"}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);
        let error = client
            .site_replication_resync(
                SiteReplicationResyncOperation::Start,
                &site_replication_resync_peer(),
            )
            .await
            .expect_err("requested mutation must require an operation ID");
        assert!(matches!(error, Error::General(_)));
        assert_eq!(error.exit_code(), 1);
        assert!(!error.to_string().contains("DO-NOT-ECHO"));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_resync_accepts_not_found_without_id_and_future_wire_values() {
        for response in [
            r#"{"op":"status","status":"not-found"}"#,
            r#"{"op":"future-operation","status":"future-status","future":true}"#,
        ] {
            let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", response);
            let client = admin_client_for_endpoint(&endpoint);
            let status = client
                .site_replication_resync(
                    SiteReplicationResyncOperation::Status,
                    &site_replication_resync_peer(),
                )
                .await
                .expect("valid future-compatible resync response");

            let value = serde_json::to_value(status).expect("resync status serializes");
            let expected: serde_json::Value =
                serde_json::from_str(response).expect("valid expected JSON");
            assert_eq!(value["op"], expected["op"]);
            assert_eq!(value["status"], expected["status"]);
            if expected.get("future").is_some() {
                assert_eq!(value["future"], true);
            }
            handle.join().expect("server thread should finish");
        }
    }

    #[tokio::test]
    async fn site_replication_resync_requires_mutations_to_echo_the_requested_operation() {
        for (operation, response_operation) in [
            (SiteReplicationResyncOperation::Start, "cancel"),
            (SiteReplicationResyncOperation::Cancel, "start"),
        ] {
            let response =
                format!(r#"{{"op":"{response_operation}","id":"resync-1","status":"success"}}"#);
            let leaked: &'static str = Box::leak(response.into_boxed_str());
            let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", leaked);
            let client = admin_client_for_endpoint(&endpoint);
            let error = client
                .site_replication_resync(operation, &site_replication_resync_peer())
                .await
                .expect_err("mismatched mutation operation must fail");

            assert!(matches!(error, Error::General(_)));
            assert!(error.to_string().contains("outcome is unknown"));
            assert!(error.to_string().contains("do not retry blindly"));
            handle.join().expect("server thread should finish");
        }

        let (endpoint, _receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"op":"start","id":"resync-1","status":"success"}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);
        client
            .site_replication_resync(
                SiteReplicationResyncOperation::Status,
                &site_replication_resync_peer(),
            )
            .await
            .expect("status may return the persisted start snapshot");
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_resync_rejects_oversized_request_before_network() {
        let client = admin_client_for_endpoint("http://127.0.0.1:1");
        let mut peer = site_replication_resync_peer();
        peer.set_ca_cert_pem("x".repeat(MAX_SITE_REPLICATION_REQUEST_BYTES + 1));

        let error = client
            .site_replication_resync(SiteReplicationResyncOperation::Start, &peer)
            .await
            .expect_err("oversized resync request must fail before connecting");

        assert!(matches!(error, Error::RequestRejected(_)));
        assert!(error.to_string().contains("exceeds"));
    }

    #[tokio::test]
    async fn site_replication_resync_enforces_success_and_error_response_bounds() {
        let success_response = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n",
            MAX_SITE_REPLICATION_SUCCESS_RESPONSE_BYTES + 1
        )
        .into_bytes();
        let (endpoint, handle) = start_admin_raw_response_server(success_response);
        let client = admin_client_for_endpoint(&endpoint);
        let peer = site_replication_resync_peer();
        let error = client
            .site_replication_resync(SiteReplicationResyncOperation::Status, &peer)
            .await
            .expect_err("declared oversized success must fail");
        assert!(matches!(error, Error::General(_)));
        assert!(error.to_string().contains("exceeds"));
        assert!(!error.to_string().contains("outcome is unknown"));
        handle.join().expect("server thread should finish");

        let mut error_response = b"HTTP/1.1 500 Internal Server Error\r\ntransfer-encoding: chunked\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n".to_vec();
        let body = vec![b'x'; MAX_SITE_REPLICATION_ERROR_RESPONSE_BYTES + 1];
        error_response.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
        error_response.extend_from_slice(&body);
        error_response.extend_from_slice(b"\r\n0\r\n\r\n");
        let (endpoint, handle) = start_admin_raw_response_server(error_response);
        let client = admin_client_for_endpoint(&endpoint);
        let error = client
            .site_replication_resync(SiteReplicationResyncOperation::Start, &peer)
            .await
            .expect_err("chunked oversized error must fail");
        assert!(matches!(error, Error::General(_)));
        assert!(error.to_string().contains("exceeds"));
        assert!(error.to_string().contains("outcome is unknown"));
        assert!(error.to_string().contains("do not retry blindly"));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn realtime_metrics_rejects_malformed_and_oversized_records() {
        let (endpoint, _receiver, handle) = start_admin_owned_test_server(
            "200 OK",
            "application/x-ndjson",
            "{not-json}\n".to_string(),
        );
        let client = admin_client_for_endpoint(&endpoint);
        let error = client
            .realtime_metrics(&rc_core::admin::MetricsQuery::default())
            .await
            .expect_err("malformed metrics should fail");
        assert!(matches!(error, Error::Json(_)));
        handle.join().expect("server thread should finish");

        let oversized = format!(
            "{{\"padding\":\"{}\"}}\n",
            "x".repeat(rc_core::admin::MAX_METRICS_LINE_BYTES)
        );
        let (endpoint, _receiver, handle) =
            start_admin_owned_test_server("200 OK", "application/x-ndjson", oversized);
        let client = admin_client_for_endpoint(&endpoint);
        let error = client
            .realtime_metrics(&rc_core::admin::MetricsQuery::default())
            .await
            .expect_err("oversized metrics should fail");
        assert!(matches!(error, Error::General(message) if message.contains("record limit")));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_resync_mutation_rejects_malformed_success_as_unknown() {
        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", "{not-json");
        let client = admin_client_for_endpoint(&endpoint);
        let error = client
            .site_replication_resync(
                SiteReplicationResyncOperation::Start,
                &site_replication_resync_peer(),
            )
            .await
            .expect_err("malformed mutation response must fail");

        assert!(matches!(error, Error::General(_)));
        assert!(error.to_string().contains("outcome is unknown"));
        assert!(error.to_string().contains("do not retry blindly"));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_resync_mutation_server_error_is_an_unknown_outcome() {
        for operation in [
            SiteReplicationResyncOperation::Start,
            SiteReplicationResyncOperation::Cancel,
        ] {
            let (endpoint, _receiver, handle) = start_admin_test_server(
                "500 Internal Server Error",
                r#"{"message":"snapshot save failed"}"#,
            );
            let client = admin_client_for_endpoint(&endpoint);
            let error = client
                .site_replication_resync(operation, &site_replication_resync_peer())
                .await
                .expect_err("mutation server error must report an unknown outcome");

            assert!(matches!(error, Error::Network(_)));
            assert!(error.to_string().contains("outcome is unknown"));
            assert!(error.to_string().contains("do not retry blindly"));
            assert!(!error.to_string().contains("snapshot save failed"));
            handle.join().expect("server thread should finish");
        }
    }

    #[tokio::test]
    async fn site_replication_resync_does_not_follow_redirects() {
        for operation in [
            SiteReplicationResyncOperation::Start,
            SiteReplicationResyncOperation::Status,
            SiteReplicationResyncOperation::Cancel,
        ] {
            let response = b"HTTP/1.1 307 Temporary Redirect\r\nlocation: http://127.0.0.1:1/replayed\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec();
            let (endpoint, handle) = start_admin_raw_response_server(response);
            let client = admin_client_for_endpoint(&endpoint);
            let error = client
                .site_replication_resync(operation, &site_replication_resync_peer())
                .await
                .expect_err("resync redirect must not be followed");

            assert!(matches!(error, Error::Network(_)));
            assert!(error.to_string().contains("HTTP 307"));
            handle.join().expect("server thread should finish");
        }
    }

    #[tokio::test]
    async fn site_replication_resync_start_and_cancel_disconnects_have_specific_unknown_outcomes() {
        for (operation, label) in [
            (SiteReplicationResyncOperation::Start, "resync start"),
            (SiteReplicationResyncOperation::Cancel, "resync cancel"),
        ] {
            let expected_target = format!(
                "/rustfs/admin/v3/site-replication/resync/op?operation={}",
                operation.as_str()
            );
            let (endpoint, receiver, handle) = start_admin_disconnect_server();
            let client = admin_client_for_endpoint(&endpoint);
            let error = client
                .site_replication_resync(operation, &site_replication_resync_peer())
                .await
                .expect_err("mutation disconnect must report unknown outcome");

            assert!(matches!(error, Error::Network(_)));
            assert_eq!(error.exit_code(), 3);
            assert!(error.to_string().contains(label));
            assert!(error.to_string().contains("outcome is unknown"));
            assert!(error.to_string().contains("not retried"));
            let request = receiver.recv().expect("single captured request");
            assert_eq!(request.method, "PUT");
            assert_eq!(request.target, expected_target);
            handle.join().expect("server thread should finish");
        }
    }

    #[tokio::test]
    async fn site_replication_resync_cancel_read_disconnect_has_specific_unknown_outcome() {
        let response = b"HTTP/1.1 200 OK\r\ncontent-length: 128\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"op\":\"cancel\"".to_vec();
        let (endpoint, handle) = start_admin_raw_response_server(response);
        let client = admin_client_for_endpoint(&endpoint);
        let error = client
            .site_replication_resync(
                SiteReplicationResyncOperation::Cancel,
                &site_replication_resync_peer(),
            )
            .await
            .expect_err("truncated cancel response must fail");

        assert!(matches!(error, Error::Network(_)));
        assert!(error.to_string().contains("resync cancel"));
        assert!(error.to_string().contains("outcome is unknown"));
        assert!(error.to_string().contains("response could not be read"));
        assert!(error.to_string().contains("not retried"));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_resync_status_disconnect_is_retryable_network_semantics() {
        let (endpoint, receiver, handle) = start_admin_disconnect_server();
        let client = admin_client_for_endpoint(&endpoint);
        let error = client
            .site_replication_resync(
                SiteReplicationResyncOperation::Status,
                &site_replication_resync_peer(),
            )
            .await
            .expect_err("status disconnect must fail");

        assert!(matches!(error, Error::Network(_)));
        assert_eq!(error.exit_code(), 3);
        assert!(!error.to_string().contains("outcome is unknown"));
        assert!(!error.to_string().contains("not retried"));
        let request = receiver.recv().expect("single captured request");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/site-replication/resync/op?operation=status"
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn site_replication_resync_maps_route_and_state_errors() {
        for (status, expected_exit) in [
            ("401 Unauthorized", 4),
            ("403 Forbidden", 4),
            ("404 Not Found", 7),
            ("405 Method Not Allowed", 7),
            ("501 Not Implemented", 7),
            ("409 Conflict", 6),
        ] {
            let (endpoint, _receiver, handle) = start_admin_test_server(status, "{}");
            let client = admin_client_for_endpoint(&endpoint);
            let error = client
                .site_replication_resync(
                    SiteReplicationResyncOperation::Status,
                    &site_replication_resync_peer(),
                )
                .await
                .expect_err("mapped resync response must fail");

            assert_eq!(error.exit_code(), expected_exit, "HTTP status {status}");
            handle.join().expect("server thread should finish");
        }

        let (endpoint, _receiver, handle) = start_admin_test_server(
            "400 Bad Request",
            r#"{"code":"SiteReplicationOperationPending","message":"site replication operation pending"}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);
        let error = client
            .site_replication_resync(
                SiteReplicationResyncOperation::Start,
                &site_replication_resync_peer(),
            )
            .await
            .expect_err("pending resync must conflict");
        assert_eq!(error.exit_code(), 6);
        handle.join().expect("server thread should finish");

        for message in [
            "no resync in progress",
            "invalid peer specified - cannot resync to self",
            "site replication peer not found",
        ] {
            let body = format!(r#"{{"code":"InvalidRequest","message":"{message}"}}"#);
            let leaked: &'static str = Box::leak(body.into_boxed_str());
            let (endpoint, _receiver, handle) = start_admin_test_server("400 Bad Request", leaked);
            let client = admin_client_for_endpoint(&endpoint);
            let error = client
                .site_replication_resync(
                    SiteReplicationResyncOperation::Status,
                    &site_replication_resync_peer(),
                )
                .await
                .expect_err("known resync invalid state must conflict");

            assert_eq!(error.exit_code(), 6, "message {message}");
            handle.join().expect("server thread should finish");
        }

        let (endpoint, _receiver, handle) = start_admin_test_server(
            "400 Bad Request",
            r#"{"code":"InvalidRequest","message":"resync option is malformed"}"#,
        );
        let client = admin_client_for_endpoint(&endpoint);
        let error = client
            .site_replication_resync(
                SiteReplicationResyncOperation::Status,
                &site_replication_resync_peer(),
            )
            .await
            .expect_err("arbitrary bad request must remain general");
        assert_eq!(error.exit_code(), 1);
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn test_update_service_account_posts_access_key_and_partial_body() {
        let (endpoint, receiver, handle) = start_admin_test_server("204 No Content", "");
        let client = admin_client_for_endpoint(&endpoint);

        client
            .update_service_account(
                "service key/one",
                rc_core::admin::UpdateServiceAccountRequest {
                    new_policy: Some(r#"{"Version":"2012-10-17"}"#.to_string()),
                    new_description: Some("Updated description".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("update service account request");

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/update-service-account?accessKey=service%20key%2Fone"
        );

        let body: serde_json::Value =
            serde_json::from_slice(&request.body).expect("update body should be JSON");
        assert_eq!(body["newPolicy"], r#"{"Version":"2012-10-17"}"#);
        assert_eq!(body["newDescription"], "Updated description");
        assert_eq!(body.as_object().expect("request object").len(), 2);
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn diagnostic_read_routes_use_typed_bounded_snapshots() {
        let health = r#"{"version":"1.0.0-beta.10","cpu":{"logical_cores":8,"brand":"test-cpu","frequency_mhz":2400,"usage_percent":12.5},"memory":{"total_bytes":1024,"used_bytes":512,"available_bytes":512,"total_swap_bytes":0,"used_swap_bytes":0},"os":{"os":"linux","kernel_version":"6.8","os_version":"test","hostname":"node-1","arch":"x86_64","uptime_secs":60},"process":{"pid":42,"cpu_usage_percent":1.25,"memory_bytes":128},"drives":[],"unsupported_probes":["perf-net"]}"#;
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", health);
        let client = admin_client_for_endpoint(&endpoint);
        let snapshot = client
            .health_snapshot()
            .await
            .expect("health snapshot should succeed");
        assert_eq!(snapshot.unsupported_probes, ["perf-net"]);
        assert_eq!(
            receiver.recv().expect("health request").target,
            "/rustfs/admin/v3/healthinfo"
        );
        handle.join().expect("health server should finish");

        let (endpoint, receiver, handle) =
            start_admin_test_server("200 OK", r#"{"snapshot":null}"#);
        let client = admin_client_for_endpoint(&endpoint);
        let snapshot = client
            .cluster_snapshot()
            .await
            .expect("null cluster snapshot should succeed");
        assert!(snapshot.snapshot.is_none());
        assert_eq!(
            receiver.recv().expect("cluster request").target,
            "/rustfs/admin/v4/cluster/snapshot"
        );
        handle.join().expect("cluster server should finish");

        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", EXTENSIONS_RESPONSE);
        let client = admin_client_for_endpoint(&endpoint);
        let catalog = client
            .extensions_catalog()
            .await
            .expect("extension catalog should succeed");
        assert_eq!(catalog.extensions.len(), 1);
        assert_eq!(
            receiver.recv().expect("extensions request").target,
            "/rustfs/admin/v4/extensions/catalog"
        );
        handle.join().expect("extensions server should finish");
    }

    #[tokio::test]
    async fn diagnostic_reads_reject_malformed_and_oversized_json() {
        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", "{not-json}");
        let error = admin_client_for_endpoint(&endpoint)
            .health_snapshot()
            .await
            .expect_err("malformed health JSON should fail");
        assert!(matches!(error, Error::Json(_)));
        handle.join().expect("malformed server should finish");

        let (endpoint, _receiver, handle) = start_admin_content_length_test_server(
            "200 OK",
            rc_core::admin::MAX_DIAGNOSTIC_RESPONSE_BYTES + 1,
        );
        let error = admin_client_for_endpoint(&endpoint)
            .extensions_catalog()
            .await
            .expect_err("oversized catalog should fail");
        assert!(matches!(error, Error::General(message) if message.contains("8 MiB")));
        handle.join().expect("oversized server should finish");
    }

    #[tokio::test]
    async fn diagnostic_reads_reject_semantically_incomplete_json() {
        for (body, read) in [
            ("{}", DiagnosticCapability::HealthSnapshot),
            (
                r#"{"version":"1.0.0-beta.10","cpu":{},"memory":{"total_bytes":1024,"used_bytes":512,"available_bytes":512,"total_swap_bytes":0,"used_swap_bytes":0},"os":{"os":"linux","arch":"x86_64","uptime_secs":60},"process":{"pid":42,"cpu_usage_percent":1.25,"memory_bytes":128},"drives":[],"unsupported_probes":[]}"#,
                DiagnosticCapability::HealthSnapshot,
            ),
            ("{}", DiagnosticCapability::ClusterSnapshot),
            (
                r#"{"extensions":[]}"#,
                DiagnosticCapability::ExtensionsCatalog,
            ),
        ] {
            let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", body);
            let client = admin_client_for_endpoint(&endpoint);
            let error = match read {
                DiagnosticCapability::HealthSnapshot => client
                    .health_snapshot()
                    .await
                    .expect_err("incomplete health snapshot should fail"),
                DiagnosticCapability::ClusterSnapshot => client
                    .cluster_snapshot()
                    .await
                    .expect_err("missing cluster snapshot field should fail"),
                DiagnosticCapability::ExtensionsCatalog => client
                    .extensions_catalog()
                    .await
                    .expect_err("incomplete extension catalog should fail"),
                _ => unreachable!("fixture only covers diagnostic reads"),
            };
            assert!(matches!(error, Error::Json(_)));
            handle
                .join()
                .expect("incomplete response server should finish");
        }
    }

    #[tokio::test]
    async fn diagnostic_route_errors_distinguish_permission_and_unsupported() {
        for (status, expected_auth) in [("403 Forbidden", true), ("404 Not Found", false)] {
            let (endpoint, _receiver, handle) = start_admin_test_server(
                status,
                r#"{"code":"AccessDenied","message":"denied or absent"}"#,
            );
            let error = admin_client_for_endpoint(&endpoint)
                .health_snapshot()
                .await
                .expect_err("diagnostic route should fail");
            if expected_auth {
                assert!(matches!(error, Error::Auth(_)));
            } else {
                assert!(matches!(error, Error::UnsupportedFeature(_)));
            }
            handle.join().expect("error server should finish");
        }
    }

    #[test]
    fn test_admin_client_invalid_ca_bundle_path_surfaces_error() {
        let mut alias = Alias::new("test", "https://localhost:9000", "access", "secret");
        alias.ca_bundle = Some("/definitely-not-here/ca.pem".to_string());

        let result = AdminClient::new(&alias);
        match result {
            Err(Error::Network(msg)) => {
                assert!(
                    msg.contains("Failed to read CA bundle"),
                    "Unexpected error message: {msg}"
                );
            }
            Ok(_) => panic!("Expected Error::Network for invalid path, got Ok(_)"),
            Err(e) => panic!("Expected Error::Network for invalid path, got Err({e})"),
        }
    }

    #[test]
    fn test_admin_client_invalid_ca_bundle_pem_surfaces_error() {
        let temp_dir = tempdir().expect("create temp dir");
        let bad_pem_path = temp_dir.path().join("bad-ca.pem");
        std::fs::write(
            &bad_pem_path,
            b"-----BEGIN CERTIFICATE-----\ninvalid-base64\n-----END CERTIFICATE-----\n",
        )
        .expect("write invalid PEM");

        let mut alias = Alias::new("test", "https://localhost:9000", "access", "secret");
        alias.ca_bundle = Some(bad_pem_path.display().to_string());

        let result = AdminClient::new(&alias);
        match result {
            Err(Error::Network(msg)) => {
                assert!(
                    msg.contains("Invalid CA bundle") && msg.contains("bad-ca.pem"),
                    "Unexpected error message for invalid PEM CA bundle: {msg}"
                );
            }
            Ok(_) => panic!("Expected Error::Network for invalid PEM, got Ok(_)"),
            Err(e) => panic!("Expected Error::Network for invalid PEM, got Err({e})"),
        }
    }

    #[tokio::test]
    async fn replication_diff_posts_empty_signed_body_and_preserves_extensions() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{
                "Entries":[{
                    "Object":"reports/a.json",
                    "VersionID":"v1",
                    "Size":42,
                    "IsDeleteMarker":false,
                    "ReplicationStatus":"FAILED",
                    "LastModified":"2026-07-21T04:00:00Z",
                    "TargetDetail":{"attempts":2}
                }],
                "IsTruncated":false,
                "ScannedVersions":24,
                "ServerRevision":7
            }"#,
        );
        let client = admin_client_for_endpoint(&endpoint);

        let diff = client
            .replication_diff("source bucket", Some("reports/2026 Q3/"))
            .await
            .expect("replication diff");

        assert_eq!(diff.entries[0].object, "reports/a.json");
        assert_eq!(diff.entries[0].extra["TargetDetail"]["attempts"], 2);
        assert_eq!(diff.extra["ServerRevision"], 7);
        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/replication/diff?bucket=source%20bucket&prefix=reports%2F2026%20Q3%2F"
        );
        assert!(request.body.is_empty());
        assert!(request.headers.to_ascii_lowercase().contains(
            "x-amz-content-sha256: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        ));
        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn replication_diff_omits_prefix_query_when_not_requested() {
        let (endpoint, receiver, handle) = start_admin_test_server(
            "200 OK",
            r#"{"Entries":[],"IsTruncated":false,"ScannedVersions":0}"#,
        );
        let client = anonymous_admin_client_for_endpoint(&endpoint);

        let diff = client
            .replication_diff("source", None)
            .await
            .expect("empty replication diff");

        assert!(diff.entries.is_empty());
        assert_eq!(
            receiver.recv().expect("captured request").target,
            "/rustfs/admin/v3/replication/diff?bucket=source"
        );
        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn replication_diff_maps_auth_missing_and_unsupported_responses() {
        let cases = [
            (
                "403 Forbidden",
                r#"{"Code":"AccessDenied","Message":"denied"}"#,
                "auth",
            ),
            (
                "404 Not Found",
                r#"{"Code":"NoSuchBucket","Message":"missing bucket"}"#,
                "not_found",
            ),
            (
                "404 Not Found",
                r#"{"Code":"ReplicationConfigurationNotFoundError","Message":"replication is not configured"}"#,
                "not_found",
            ),
            (
                "404 Not Found",
                r#"{"message":"route missing"}"#,
                "unsupported",
            ),
            (
                "501 Not Implemented",
                r#"{"Code":"NotImplemented","Message":"not implemented"}"#,
                "unsupported",
            ),
        ];

        for (status, body, expected) in cases {
            let (endpoint, _receiver, handle) = start_admin_test_server(status, body);
            let error = anonymous_admin_client_for_endpoint(&endpoint)
                .replication_diff("source", None)
                .await
                .expect_err("HTTP error response");
            match expected {
                "auth" => assert!(matches!(error, Error::Auth(_))),
                "not_found" => assert!(matches!(error, Error::NotFound(_))),
                "unsupported" => assert!(matches!(error, Error::UnsupportedFeature(_))),
                _ => panic!("unexpected test expectation"),
            }
            handle.join().expect("server thread");
        }
    }

    #[tokio::test]
    async fn replication_diff_rejects_malformed_json_and_network_failure() {
        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", "not-json");
        let malformed = anonymous_admin_client_for_endpoint(&endpoint)
            .replication_diff("source", None)
            .await
            .expect_err("malformed JSON response");
        assert!(matches!(malformed, Error::Json(_)));
        handle.join().expect("server thread");

        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve unused port");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        drop(listener);
        let network = anonymous_admin_client_for_endpoint(&endpoint)
            .replication_diff("source", None)
            .await
            .expect_err("connection failure");
        assert!(matches!(network, Error::Network(_)));
    }

    #[tokio::test]
    async fn replication_diff_rejects_declared_and_chunked_overflow() {
        let (endpoint, _receiver, handle) = start_admin_declared_length_server(
            "403 Forbidden",
            MAX_REPLICATION_DIFF_RESPONSE_BYTES + 1,
        );
        let declared = anonymous_admin_client_for_endpoint(&endpoint)
            .replication_diff("source", None)
            .await
            .expect_err("declared overflow");
        assert!(matches!(declared, Error::General(message) if message.contains("response limit")));
        handle.join().expect("server thread");

        let (endpoint, _receiver, completion) = start_admin_chunked_overflow_server();
        let chunked = anonymous_admin_client_for_endpoint(&endpoint)
            .replication_diff("source", None)
            .await
            .expect_err("chunked overflow");
        assert!(matches!(chunked, Error::General(message) if message.contains("response limit")));
        completion
            .recv_timeout(Duration::from_secs(5))
            .expect("chunked overflow server should complete within its socket timeout");
    }

    #[tokio::test]
    async fn replication_metrics_reads_typed_truth_and_redacts_credentials() {
        let body = r#"{
            "stats":{"arn:secret":{"replicated_size":30,"replicated_count":3,
                "failed":{"count":2,"size":20},"fail_stats":{"count":2,"size":20},
                "latency":{"avg":1.0,"curr":2.0,"max":3.0},
                "xfer_rate_lrg":{"avg":4.0,"curr":5.0,"peak":6.0},
                "xfer_rate_sml":{"avg":7.0,"curr":8.0,"peak":9.0},
                "bandwidth_limit_bytes_per_sec":100,
                "current_bandwidth_bytes_per_sec":50.0,
                "latency_scope":"partial_cluster","bandwidth_scope":"node_local",
                "Future":"access"}},
            "replica_size":40,"replica_count":4,"replicated_size":30,"replicated_count":3,
            "q_stat":{"curr":{"count":1,"bytes":10},"avg":{"count":1,"bytes":10},
                "max":{"count":2,"bytes":20},"last_minute":{"count":1,"bytes":10}},
            "provider_available":true,"cluster_complete":false,
            "observed_node_count":1,"expected_node_count":2,"queue_scope":"partial_cluster"
        }"#;
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", body);
        let client = admin_client_for_endpoint(&endpoint);

        let metrics = client
            .replication_metrics("source bucket")
            .await
            .expect("typed metrics");

        assert_eq!(metrics.provider_available, Some(true));
        assert_eq!(metrics.q_stat.curr.size, 10);
        assert!(metrics.stats.contains_key("arn:[REDACTED]"));
        assert_eq!(
            metrics.stats["arn:[REDACTED]"].extra["Future"],
            "[REDACTED]"
        );
        assert_eq!(
            receiver.recv().expect("request").target,
            "/rustfs/admin/v3/replicationmetrics?bucket=source%20bucket"
        );
        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn replication_mrf_preserves_partial_and_explicit_availability() {
        let body = r#"{"Bucket":"source","Targets":[
            {"ARN":"arn:a","FailedCount":2,"FailedSize":20,"ObservationScope":"partial_cluster"}
        ],"TotalFailedCount":2,"TotalFailedSize":20,"QueuedCount":1,"QueuedSize":10,
        "PerObjectEntriesAvailable":false,"RuntimeStatsAvailable":true,
        "ClusterComplete":false,"ObservedNodeCount":1,"ExpectedNodeCount":2,
        "DurableBacklogAvailable":true,"DurableCount":3,"DurableSize":30,
        "PerTargetDurableEntriesAvailable":false}"#;
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", body);

        let mrf = anonymous_admin_client_for_endpoint(&endpoint)
            .replication_mrf("source")
            .await
            .expect("typed MRF");

        assert!(!mrf.cluster_complete);
        assert!(!mrf.per_object_entries_available);
        assert_eq!(mrf.queued_count, 1);
        assert_eq!(
            receiver.recv().expect("request").target,
            "/rustfs/admin/v3/replication/mrf?bucket=source"
        );
        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn replication_inspection_distinguishes_errors_and_rejects_bad_truth() {
        for (status, body, expected) in [
            (
                "403 Forbidden",
                r#"{"Code":"AccessDenied","Message":"denied"}"#,
                "auth",
            ),
            (
                "404 Not Found",
                r#"{"Code":"ReplicationConfigurationNotFoundError","Message":"absent"}"#,
                "not_found",
            ),
            (
                "404 Not Found",
                r#"{"message":"route absent"}"#,
                "unsupported",
            ),
        ] {
            let (endpoint, _receiver, handle) = start_admin_test_server(status, body);
            let error = anonymous_admin_client_for_endpoint(&endpoint)
                .replication_metrics("source")
                .await
                .expect_err("inspection error");
            match expected {
                "auth" => assert!(matches!(error, Error::Auth(_))),
                "not_found" => assert!(matches!(error, Error::NotFound(_))),
                "unsupported" => assert!(matches!(error, Error::UnsupportedFeature(_))),
                _ => unreachable!(),
            }
            handle.join().expect("server thread");
        }

        let (endpoint, _receiver, handle) =
            start_admin_test_server("404 Not Found", r#"{"message":"route access absent"}"#);
        let redacted = admin_client_for_endpoint(&endpoint)
            .replication_metrics("source")
            .await
            .expect_err("unsupported response");
        assert!(!redacted.to_string().contains("access"));
        assert!(redacted.to_string().contains("[REDACTED]"));
        handle.join().expect("server thread");

        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", "not-json");
        assert!(matches!(
            anonymous_admin_client_for_endpoint(&endpoint)
                .replication_metrics("source")
                .await
                .expect_err("malformed response"),
            Error::Json(_)
        ));
        handle.join().expect("server thread");

        let inconsistent = r#"{"Bucket":"source","Targets":[{"ARN":"a","FailedCount":1,"FailedSize":1,"ObservationScope":"node_local"}],"TotalFailedCount":2,"TotalFailedSize":1,"QueuedCount":0,"QueuedSize":0,"PerObjectEntriesAvailable":false,"RuntimeStatsAvailable":true,"ClusterComplete":true,"ObservedNodeCount":1,"ExpectedNodeCount":1,"DurableBacklogAvailable":false,"DurableCount":0,"DurableSize":0,"PerTargetDurableEntriesAvailable":false}"#;
        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", inconsistent);
        assert!(matches!(
            anonymous_admin_client_for_endpoint(&endpoint)
                .replication_mrf("source")
                .await
                .expect_err("inconsistent totals"),
            Error::General(message) if message.contains("inconsistent")
        ));
        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn replication_inspection_bounds_error_and_success_bodies() {
        let (endpoint, _receiver, handle) = start_admin_declared_length_server(
            "200 OK",
            MAX_REPLICATION_INSPECTION_RESPONSE_BYTES + 1,
        );
        let error = anonymous_admin_client_for_endpoint(&endpoint)
            .replication_metrics("source")
            .await
            .expect_err("oversized success");
        assert!(matches!(error, Error::General(message) if message.contains("response limit")));
        handle.join().expect("server thread");

        let (endpoint, _receiver, handle) = start_admin_declared_length_server(
            "403 Forbidden",
            MAX_REPLICATION_INSPECTION_RESPONSE_BYTES + 1,
        );
        let error = anonymous_admin_client_for_endpoint(&endpoint)
            .replication_mrf("source")
            .await
            .expect_err("oversized error");
        assert!(matches!(error, Error::General(message) if message.contains("response limit")));
        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn observability_routes_distinguish_permission_denial_from_unsupported() {
        for (status, expected_auth) in [("403 Forbidden", true), ("404 Not Found", false)] {
            let (endpoint, _receiver, handle) = start_admin_test_server(
                status,
                r#"{"code":"AccessDenied","message":"denied or absent"}"#,
            );
            let client = admin_client_for_endpoint(&endpoint);
            let error = client
                .scanner_status()
                .await
                .expect_err("request should fail");
            if expected_auth {
                assert!(matches!(error, Error::Auth(_)));
            } else {
                assert!(matches!(error, Error::UnsupportedFeature(_)));
            }
            handle.join().expect("server thread should finish");
        }
    }
    #[tokio::test]
    async fn kms_status_uses_service_status_and_normalizes_safe_summary() {
        let body = r#"{"status":"Running","backend_type":"VaultKV2","healthy":true,"config_summary":{"backend_type":"VaultKV2","default_key_id":"archive-key","timeout_seconds":30,"retry_attempts":3,"enable_cache":true,"max_cached_keys":1000,"cache_ttl_seconds":3600,"cache_summary":{"max_keys":500,"ttl_seconds":900,"enable_metrics":true},"backend_summary":{"backend_type":"vault-kv2","address":"https://user:secret@vault.example/path?token=hidden#fragment","auth_method_type":"approle","has_stored_credentials":true,"skip_tls_verify":false}}}"#;
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", body);
        let client = admin_client_for_endpoint(&endpoint);

        let status = client.kms_status().await.expect("KMS status should parse");

        assert_eq!(status.state, KmsServiceState::Running);
        assert_eq!(status.backend, Some(KmsBackendKind::VaultKv2));
        assert_eq!(status.healthy, Some(true));
        let config = status.config.as_ref().expect("safe config summary");
        assert_eq!(config.default_key_id.as_deref(), Some("archive-key"));
        assert_eq!(
            config.endpoint.as_deref(),
            Some("https://vault.example/path")
        );
        assert_eq!(config.auth_method.as_deref(), Some("approle"));
        assert_eq!(config.credentials_configured, Some(true));
        assert_eq!(config.cache.max_keys, Some(500));

        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/rustfs/admin/v3/kms/service-status");
        assert!(!format!("{status:?}").contains("token"));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn kms_status_treats_unconfigured_as_a_stable_result() {
        let body = r#"{"status":"NotConfigured","backend_type":null,"healthy":null,"config_summary":null}"#;
        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", body);
        let client = admin_client_for_endpoint(&endpoint);

        let status = client
            .kms_status()
            .await
            .expect("unconfigured KMS should not be an error");

        assert_eq!(status.state, KmsServiceState::NotConfigured);
        assert_eq!(status.backend, None);
        assert_eq!(status.healthy, None);
        assert_eq!(status.config, None);
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn kms_status_redacts_server_error_details() {
        let body = r#"{"status":{"Error":"vault token MUST_NOT_APPEAR was rejected"},"backend_type":"VaultKV2","healthy":false,"config_summary":null}"#;
        let (endpoint, _receiver, handle) = start_admin_test_server("200 OK", body);
        let client = admin_client_for_endpoint(&endpoint);

        let status = client.kms_status().await.expect("error state should parse");

        assert_eq!(status.state, KmsServiceState::Error);
        assert_eq!(
            status.error_message.as_deref(),
            Some("KMS service reported an error")
        );
        assert!(!format!("{status:?}").contains("MUST_NOT_APPEAR"));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn kms_list_keys_encodes_pagination_and_normalizes_key_info() {
        let body = r#"{"success":true,"message":"keys listed successfully","keys":[{"key_id":"archive/key","description":"Archive key","algorithm":"AES_256","usage":"EncryptDecrypt","status":"Active","version":2,"metadata":{"owner":"storage"},"tags":{"environment":"prod"},"created_at":"2026-07-21T00:00:00Z","rotated_at":null,"created_by":"admin"}],"truncated":true,"next_marker":"next/key"}"#;
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", body);
        let client = admin_client_for_endpoint(&endpoint);

        let page = client
            .kms_list_keys(25, Some("marker/with space"))
            .await
            .expect("KMS key page should parse");

        assert!(page.truncated);
        assert_eq!(page.next_marker.as_deref(), Some("next/key"));
        assert_eq!(page.keys.len(), 1);
        assert_eq!(page.keys[0].state, KmsKeyState::Active);
        assert_eq!(page.keys[0].usage, KmsKeyUsage::EncryptDecrypt);
        assert_eq!(
            page.keys[0].tags.get("owner").map(String::as_str),
            Some("storage")
        );
        assert_eq!(
            page.keys[0].tags.get("environment").map(String::as_str),
            Some("prod")
        );

        let request = receiver.recv().expect("captured request");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/kms/keys?limit=25&marker=marker%2Fwith%20space"
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn kms_describe_key_encodes_path_segment() {
        let body = r#"{"success":true,"message":"Key described successfully","key_metadata":{"key_id":"archive/key one","key_state":"Enabled","key_usage":"EncryptDecrypt","description":null,"creation_date":"2026-07-21T00:00:00Z","deletion_date":null,"origin":"AWS_KMS","key_manager":"CUSTOMER","tags":{}}}"#;
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", body);
        let client = admin_client_for_endpoint(&endpoint);

        let key = client
            .kms_describe_key("archive/key one")
            .await
            .expect("KMS key should parse");

        assert_eq!(key.key_id, "archive/key one");
        assert_eq!(key.state, KmsKeyState::Enabled);
        let request = receiver.recv().expect("captured request");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/kms/keys/archive%2Fkey%20one"
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn kms_status_preserves_auth_unavailable_and_malformed_failures() {
        for (status, body, expected) in [
            ("403 Forbidden", r#"{"Code":"AccessDenied"}"#, "auth"),
            (
                "503 Service Unavailable",
                r#"{"message":"kms service is not running"}"#,
                "network",
            ),
            ("200 OK", r#"{"status":"Running"#, "json"),
        ] {
            let (endpoint, _receiver, handle) = start_admin_test_server(status, body);
            let client = admin_client_for_endpoint(&endpoint);
            let error = client.kms_status().await.expect_err("response should fail");
            match expected {
                "auth" => assert!(matches!(error, Error::Auth(_))),
                "network" => assert!(matches!(error, Error::Network(_))),
                "json" => assert!(matches!(error, Error::Json(_))),
                _ => panic!("unexpected expected error kind"),
            }
            handle.join().expect("server thread should finish");
        }
    }

    #[test]
    fn iam_import_wire_report_discards_server_error_text() {
        let wire: WireIamImportResult = serde_json::from_str(
            r#"{
                "added": {"users": ["bob"]},
                "failed": {
                    "users": [{
                        "name": "alice",
                        "error": "secretKey=must-not-survive"
                    }],
                    "userPolicies": [{
                        "name": "alice",
                        "policies": ["readonly"],
                        "error": "token=must-not-survive"
                    }]
                }
            }"#,
        )
        .expect("wire response");
        let safe = wire.into_safe_result();
        assert_eq!(safe.added.users, ["bob"]);
        assert_eq!(safe.failed.len(), 2);
        let encoded = serde_json::to_string(&safe).expect("safe response");
        assert!(!encoded.contains("must-not-survive"));
        assert!(!encoded.contains("secretKey"));
        assert!(!encoded.contains("token"));
    }

    #[test]
    fn iam_archive_status_errors_never_echo_response_bodies() {
        for (status, mutation) in [
            (StatusCode::FORBIDDEN, false),
            (StatusCode::BAD_REQUEST, true),
            (StatusCode::INTERNAL_SERVER_ERROR, true),
        ] {
            let error = iam_archive_status_error(status, mutation);
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[tokio::test]
    async fn iam_import_uses_v2_route_and_returns_only_safe_outcomes() {
        let body = r#"{
            "added": {"users": ["bob"]},
            "failed": {"users": [{"name": "alice", "error": "secretKey=hidden"}]}
        }"#;
        let (endpoint, receiver, handle) = start_admin_test_server("200 OK", body);
        let client = admin_client_for_endpoint(&endpoint);
        let archive = b"bounded archive".to_vec();
        let result = client
            .import_iam_archive(archive.clone())
            .await
            .expect("import result");
        assert_eq!(result.added.users, ["bob"]);
        assert_eq!(result.failed[0].name, "alice");
        assert!(
            !serde_json::to_string(&result)
                .expect("result")
                .contains("hidden")
        );
        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "PUT");
        assert_eq!(request.target, "/rustfs/admin/v3/import-iam-v2");
        assert_eq!(request.body, archive);
        assert!(
            request
                .headers
                .to_ascii_lowercase()
                .contains("content-type: application/zip")
        );
        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn iam_export_rejects_declared_oversized_body() {
        let (endpoint, _receiver, handle) =
            start_admin_declared_length_server("200 OK", MAX_IAM_ARCHIVE_BYTES + 1);
        let client = admin_client_for_endpoint(&endpoint);
        let error = client
            .export_iam_archive()
            .await
            .expect_err("oversized archive");
        assert!(!error.to_string().contains("secret"));
        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn iam_import_permission_error_does_not_echo_server_body() {
        let (endpoint, _receiver, handle) =
            start_admin_test_server("403 Forbidden", r#"{"error":"secretKey=hidden"}"#);
        let client = admin_client_for_endpoint(&endpoint);
        let error = client
            .import_iam_archive(b"archive".to_vec())
            .await
            .expect_err("permission denied");
        assert!(matches!(error, Error::Auth(_)));
        assert!(!error.to_string().contains("hidden"));
        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn iam_import_disconnect_reports_unknown_outcome_without_retry() {
        let (endpoint, receiver, handle) = start_admin_disconnect_server();
        let client = admin_client_for_endpoint(&endpoint);
        let error = client
            .import_iam_archive(b"archive".to_vec())
            .await
            .expect_err("disconnect");
        assert!(matches!(error, Error::Network(_)));
        assert!(error.to_string().contains("outcome is unknown"));
        let request = receiver.recv().expect("captured request");
        assert_eq!(request.target, "/rustfs/admin/v3/import-iam-v2");
        handle.join().expect("server thread");
    }

    fn inspect_archive_request(max_bytes: usize) -> InspectArchiveTransportRequest {
        InspectArchiveTransportRequest {
            bucket: "diagnostics".to_string(),
            object: "node/metadata.json".to_string(),
            public_key_pem: "-----BEGIN PUBLIC KEY-----\nopaque\n-----END PUBLIC KEY-----"
                .to_string(),
            max_bytes,
            timeout: Duration::from_secs(2),
        }
    }

    #[tokio::test]
    async fn inspect_archive_capability_uses_exact_v4_contract() {
        let status = r#"{"state":"supported"}"#;
        let body = format!(
            r#"{{
                "summary": {{
                    "observability": {status},
                    "userspace_profiling": {status},
                    "memory_sampling": {status},
                    "platform": {status},
                    "topology": {status},
                    "cluster_snapshot": {status}
                }},
                "inspect_archive": {{
                    "state": {status},
                    "route": "/rustfs/admin/v4/inspect/archive",
                    "archive_version": 1,
                    "content_type": "application/vnd.rustfs.inspect-archive.v1",
                    "encryption": "RSA-OAEP-SHA256+AES-256-GCM-CHUNKED",
                    "completion_contract": "authenticated-final-record-required",
                    "max_bytes": 16777216,
                    "max_duration_secs": 30,
                    "max_metadata_bytes_per_drive": 1048576
                }},
                "cluster_snapshot_path": "/rustfs/admin/v4/cluster/snapshot",
                "cluster_snapshot_summary": null,
                "topology_status": {status}
            }}"#
        );
        let (endpoint, receiver, handle) =
            start_admin_owned_test_server("200 OK", "application/json", body);
        let contract = admin_client_for_endpoint(&endpoint)
            .inspect_archive_capability()
            .await
            .expect("exact inspect archive contract");
        contract.validate().expect("valid contract");
        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/rustfs/admin/v4/runtime/capabilities");
        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn inspect_archive_transport_streams_private_bounded_response() {
        let payload = b"encrypted-frame-bytes";
        let response = [
            format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: {INSPECT_ARCHIVE_CONTENT_TYPE}\r\nx-rustfs-inspect-completion: {INSPECT_ARCHIVE_COMPLETION}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
                payload.len()
            )
            .into_bytes(),
            payload.to_vec(),
        ]
        .concat();
        let (endpoint, receiver, handle) = start_admin_captured_raw_response_server(response);
        let directory = tempdir().expect("temporary directory");
        let encrypted = admin_client_for_endpoint(&endpoint)
            .download_inspect_archive(inspect_archive_request(1024), directory.path())
            .await
            .expect("bounded response");
        assert_eq!(
            std::fs::read(encrypted.file.path()).expect("staged file"),
            payload
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                encrypted
                    .file
                    .as_file()
                    .metadata()
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let request = receiver.recv().expect("captured request");
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/rustfs/admin/v4/inspect/archive");
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("JSON body");
        assert_eq!(body["bucket"], "diagnostics");
        assert_eq!(body["object"], "node/metadata.json");
        assert!(body["public_key_pem"].as_str().is_some());
        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn inspect_archive_transport_rejects_bad_headers_limits_and_write_failures() {
        let bad_headers = b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\ncontent-type: application/octet-stream\r\nconnection: close\r\n\r\nx".to_vec();
        let (endpoint, handle) = start_admin_raw_response_server(bad_headers);
        let directory = tempdir().expect("temporary directory");
        let error = admin_client_for_endpoint(&endpoint)
            .download_inspect_archive(inspect_archive_request(1024), directory.path())
            .await
            .err()
            .expect("contract headers must match");
        assert!(matches!(error, Error::General(_)));
        handle.join().expect("server thread");

        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: 2\r\ncontent-type: {INSPECT_ARCHIVE_CONTENT_TYPE}\r\nx-rustfs-inspect-completion: {INSPECT_ARCHIVE_COMPLETION}\r\ncache-control: no-store\r\nconnection: close\r\n\r\nxx"
        )
        .into_bytes();
        let (endpoint, handle) = start_admin_raw_response_server(response);
        let error = admin_client_for_endpoint(&endpoint)
            .download_inspect_archive(inspect_archive_request(1), directory.path())
            .await
            .err()
            .expect("declared limit");
        assert!(matches!(error, Error::RequestRejected(_)));
        handle.join().expect("server thread");

        let missing = directory.path().join("missing");
        let (endpoint, handle) = start_admin_raw_response_server(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-length: 0\r\ncontent-type: {INSPECT_ARCHIVE_CONTENT_TYPE}\r\nx-rustfs-inspect-completion: {INSPECT_ARCHIVE_COMPLETION}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n"
            )
            .into_bytes(),
        );
        let error = admin_client_for_endpoint(&endpoint)
            .download_inspect_archive(inspect_archive_request(1024), &missing)
            .await
            .err()
            .expect("staging write failure");
        assert!(matches!(error, Error::InvalidPath(_)));
        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn inspect_archive_errors_are_classified_without_echoing_server_secrets() {
        for (status, expected) in [
            ("403 Forbidden", "auth"),
            ("404 Not Found", "unsupported"),
            ("413 Payload Too Large", "limit"),
            ("504 Gateway Timeout", "timeout"),
        ] {
            let leaked = r#"{"message":"secretKey=do-not-echo"}"#;
            let (endpoint, _receiver, handle) = start_admin_test_server(status, leaked);
            let directory = tempdir().expect("temporary directory");
            let error = admin_client_for_endpoint(&endpoint)
                .download_inspect_archive(inspect_archive_request(1024), directory.path())
                .await
                .err()
                .expect(expected);
            assert!(!error.to_string().contains("do-not-echo"));
            match expected {
                "auth" => assert!(matches!(error, Error::Auth(_))),
                "unsupported" => assert!(matches!(error, Error::UnsupportedFeature(_))),
                "limit" => assert!(matches!(error, Error::RequestRejected(_))),
                "timeout" => assert!(matches!(error, Error::Network(_))),
                _ => unreachable!(),
            }
            handle.join().expect("server thread");
        }
    }
}
