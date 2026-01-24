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
    AdminApi, ClusterInfo, CreateServiceAccountRequest, Group, GroupStatus, HealStartRequest,
    HealStatus, Policy, PolicyEntity, PolicyInfo, ServiceAccount, UpdateGroupMembersRequest, User,
    UserStatus,
};
use rc_core::{Alias, Error, Result};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::SystemTime;

/// Admin API client for RustFS/MinIO-compatible servers
pub struct AdminClient {
    http_client: Client,
    endpoint: String,
    access_key: String,
    secret_key: String,
    region: String,
}

impl AdminClient {
    /// Create a new AdminClient from an Alias
    pub fn new(alias: &Alias) -> Result<Self> {
        let http_client = Client::builder()
            .danger_accept_invalid_certs(alias.insecure)
            .build()
            .map_err(|e| Error::Network(format!("Failed to create HTTP client: {e}")))?;

        Ok(Self {
            http_client,
            endpoint: alias.endpoint.trim_end_matches('/').to_string(),
            access_key: alias.access_key.clone(),
            secret_key: alias.secret_key.clone(),
            region: alias.region.clone(),
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

    /// Sign a request using AWS SigV4
    async fn sign_request(
        &self,
        method: &Method,
        url: &str,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<HeaderMap> {
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
        let content_hash = Self::sha256_hash(body_bytes);

        let mut headers = HeaderMap::new();
        headers.insert("x-amz-content-sha256", content_hash.parse().unwrap());
        headers.insert("host", self.get_host().parse().unwrap());

        if !body_bytes.is_empty() {
            headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        }

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
        let content_hash = Self::sha256_hash(body_bytes);

        let mut headers = HeaderMap::new();
        headers.insert("x-amz-content-sha256", content_hash.parse().unwrap());
        headers.insert("host", self.get_host().parse().unwrap());

        if !body_bytes.is_empty() {
            headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        }

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
            StatusCode::BAD_REQUEST => Error::InvalidPath(body.to_string()),
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
}

/// Request body for set policy
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetPolicyApiRequest {
    policy_name: String,
    entity_name: String,
}

#[async_trait]
impl AdminApi for AdminClient {
    // ==================== Cluster Operations ====================

    async fn cluster_info(&self) -> Result<ClusterInfo> {
        self.request(Method::GET, "/info", None, None).await
    }

    async fn heal_status(&self) -> Result<HealStatus> {
        self.request(Method::GET, "/heal/status", None, None).await
    }

    async fn heal_start(&self, request: HealStartRequest) -> Result<HealStatus> {
        let body = serde_json::to_vec(&request).map_err(Error::Json)?;
        self.request(Method::POST, "/heal/start", None, Some(&body))
            .await
    }

    async fn heal_stop(&self) -> Result<()> {
        self.request_no_response(Method::POST, "/heal/stop", None, None)
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
        self.request_no_response(Method::POST, "/add-canned-policy", Some(&query), Some(body))
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
        let entity_type_str = match entity_type {
            PolicyEntity::User => "user",
            PolicyEntity::Group => "group",
        };

        let body = serde_json::to_vec(&SetPolicyApiRequest {
            policy_name,
            entity_name: entity_name.to_string(),
        })
        .map_err(Error::Json)?;

        let query = [("entityType", entity_type_str)];
        self.request_no_response(Method::PUT, "/set-policy", Some(&query), Some(&body))
            .await
    }

    async fn detach_policy(
        &self,
        policy_names: &[String],
        entity_type: PolicyEntity,
        entity_name: &str,
    ) -> Result<()> {
        // Detach by setting empty policy
        // In RustFS/MinIO, you typically set a new policy which replaces the old one
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
            })
            .collect())
    }

    async fn get_service_account(&self, access_key: &str) -> Result<ServiceAccount> {
        let query = [("accessKey", access_key)];
        let response: ServiceAccount = self
            .request(Method::GET, "/info-service-account", Some(&query), None)
            .await?;
        Ok(response)
    }

    async fn create_service_account(
        &self,
        request: CreateServiceAccountRequest,
    ) -> Result<ServiceAccount> {
        let body = serde_json::to_vec(&request).map_err(Error::Json)?;
        let response: ServiceAccount = self
            .request(Method::PUT, "/add-service-account", None, Some(&body))
            .await?;
        Ok(response)
    }

    async fn delete_service_account(&self, access_key: &str) -> Result<()> {
        let query = [("accessKey", access_key)];
        self.request_no_response(
            Method::DELETE,
            "/delete-service-account",
            Some(&query),
            None,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
