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
}
