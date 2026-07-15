//! Service control and site replication admin types.
//!
//! Wire formats mirror the RustFS server's `crates/madmin` definitions
//! (MinIO-admin-compatible JSON field names).

use serde::{Deserialize, Serialize};

/// A peer site definition for `site-replication/add`.
///
/// Field names follow the MinIO admin wire format (note: the endpoint
/// serializes as `endpoints`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
