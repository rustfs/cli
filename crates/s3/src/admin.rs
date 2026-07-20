//! Admin API client implementation
//!
//! This module provides the AdminClient that implements the AdminApi trait
//! using HTTP requests with AWS SigV4 signing.

use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SignatureLocation, SigningSettings, sign,
};
use aws_sigv4::sign::v4;
use rc_core::admin::{
    AccessKeyInfo, AdminApi, BucketQuota, ClusterInfo, CreateServiceAccountRequest,
    DecommissionPoolStatus, DecommissionStatus, Group, GroupStatus, HealRuntimeState, HealScanMode,
    HealStartRequest, HealStatus, HealTaskRequest, PeerSiteSpec, Policy, PolicyEntity, PolicyInfo,
    PoolStatus, PoolTarget, RebalanceStartResult, RebalanceStatus, ServiceAccount,
    ServiceAccountCreateResponse, ServiceActionResult, SiteRemoveSpec, SiteStatusOptions,
    UpdateGroupMembersRequest, UpdateServiceAccountRequest, User, UserStatus,
};
use rc_core::{Alias, Error, Result};
use reqwest::header::{CONTENT_TYPE, HOST, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

/// Admin API client for RustFS servers
pub struct AdminClient {
    http_client: Client,
    endpoint: String,
    access_key: String,
    secret_key: String,
    region: String,
    anonymous: bool,
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

        if let Some(bundle_path) = alias.ca_bundle.as_deref() {
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
        }

        if let (Some(cert_path), Some(key_path)) =
            (alias.client_cert.as_deref(), alias.client_key.as_deref())
        {
            let mut identity_pem = std::fs::read(cert_path).map_err(|e| {
                Error::Network(format!(
                    "Failed to read client certificate '{cert_path}': {e}"
                ))
            })?;
            let key_pem = std::fs::read(key_path).map_err(|e| {
                Error::Network(format!("Failed to read client key '{key_path}': {e}"))
            })?;
            identity_pem.extend_from_slice(b"\n");
            identity_pem.extend_from_slice(&key_pem);
            let identity = reqwest::Identity::from_pem(&identity_pem).map_err(|e| {
                Error::Network(format!("Invalid client certificate/key identity: {e}"))
            })?;
            builder = builder.use_rustls_tls().identity(identity);
        }

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
        })
    }

    /// Build the base URL for admin API
    fn admin_url(&self, path: &str) -> String {
        format!("{}/rustfs/admin/v3{}", self.endpoint, path)
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

        let signable_body = SignableBody::Bytes(body);

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
    async fn request<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        query: Option<&[(&str, &str)]>,
        body: Option<&[u8]>,
    ) -> Result<T> {
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

    /// Extract host from endpoint
    fn get_host(&self) -> String {
        self.endpoint
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .to_string()
    }

    /// Map HTTP status codes to appropriate errors
    fn map_error(&self, status: StatusCode, body: &str) -> Error {
        match status {
            StatusCode::NOT_FOUND => Error::NotFound(body.to_string()),
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => Error::Auth(body.to_string()),
            StatusCode::CONFLICT => Error::Conflict(body.to_string()),
            StatusCode::BAD_REQUEST => Error::General(format!("Bad request: {body}")),
            _ => Error::Network(format!("HTTP {}: {}", status.as_u16(), body)),
        }
    }
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
        // Detach by setting empty policy
        // RustFS replaces the previous policy association when a new one is set.
        // For detach, we need to get current policies and remove the specified ones
        let _ = (policy_names, entity_type, entity_name);
        Err(Error::UnsupportedFeature(
            "Policy detach not directly supported. Use attach with remaining policies instead."
                .to_string(),
        ))
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

    async fn replication_metrics(&self, bucket: &str) -> Result<serde_json::Value> {
        let query: &[(&str, &str)] = &[("bucket", bucket)];
        self.request(Method::GET, "/replicationmetrics", Some(query), None)
            .await
    }

    async fn service_action(&self, action: &str) -> Result<ServiceActionResult> {
        let query: &[(&str, &str)] = &[("action", action)];
        self.request(Method::POST, "/service", Some(query), None)
            .await
    }

    async fn site_replication_info(&self) -> Result<serde_json::Value> {
        self.request(Method::GET, "/site-replication/info", None, None)
            .await
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
