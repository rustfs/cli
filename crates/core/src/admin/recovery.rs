//! Bounded MRF observations and independent integrity jobs.

use crate::{Error, Result};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MrfObservation {
    pub contract_version: u32,
    pub topology_fingerprint: String,
    pub observation_started_at_ms: u64,
    pub observation_finished_at_ms: u64,
    pub expected_nodes: usize,
    pub page_complete: bool,
    pub nodes: Vec<MrfNodeObservation>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MrfNodeObservation {
    pub node_id: String,
    pub host: String,
    pub state: String,
    pub reply: Option<MrfReply>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "result",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum MrfReply {
    Summary {
        summary: MrfSummary,
    },
    List {
        snapshot: MrfPage,
        observed_at_ms: u64,
    },
    Get {
        observation: MrfCurrentResponsibility,
    },
    Action {
        action: MrfActionResult,
    },
    Error {
        code: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MrfSummary {
    pub contract_version: u32,
    pub sampled_at_ms: u64,
    pub checkpoint_readable: bool,
    pub checkpoint_owner: Option<String>,
    pub checkpoint_sequence: Option<u64>,
    pub active_responsibility_count: u64,
    pub held_count: u64,
    pub legacy_generation_unknown_count: u64,
    pub bucket_incarnation_changed_count: u64,
    pub operator_accepted_count: u64,
    pub best_effort_queue_depth: u64,
    pub responsibility_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MrfPage {
    pub responsibilities: Vec<MrfResponsibility>,
    pub next_cursor: Option<String>,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MrfCurrentResponsibility {
    pub contract_version: u32,
    pub observed_at_ms: u64,
    pub responsibility: Option<MrfResponsibility>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MrfResponsibility {
    pub responsibility_id: String,
    pub bucket: String,
    pub object: String,
    pub source_bucket_incarnation_id: Option<String>,
    pub bucket_incarnation_id: Option<String>,
    pub version_id: Option<String>,
    pub status: String,
    pub accepted: Option<Value>,
    #[serde(flatten)]
    pub details: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MrfActionResult {
    pub responsibility_id: String,
    pub state: String,
    pub data_verified: bool,
    pub durable_responsibility_retained: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MrfAction {
    pub node_id: String,
    pub request: MrfActionRequest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MrfActionRequest {
    pub action: String,
    pub responsibility_id: String,
    pub expected_bucket_incarnation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledge_unverified_data_risk: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledge_unknown_source_incarnation: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledge_bucket_incarnation_mismatch: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IntegrityItem {
    pub key: String,
    /// None selects the current version; Some("null") selects the null version slot.
    pub version_id: Option<String>,
    pub expected_sha256: Option<String>,
    pub target_key: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IntegrityJobRequest {
    pub mode: String,
    pub items: Vec<IntegrityItem>,
    pub bytes_per_second: u64,
    pub max_object_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntegrityJob {
    pub format_version: u32,
    pub id: String,
    pub bucket: String,
    pub bucket_incarnation: String,
    pub request: IntegrityJobRequest,
    pub state: String,
    pub revision: u64,
    pub results: Vec<IntegrityItemResult>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntegrityItemResult {
    pub state: String,
    pub source_fingerprint: Option<String>,
    pub expected_sha256: Option<String>,
    pub evidence_source: Option<String>,
    pub source_size: Option<u64>,
    pub target_version_id: Option<String>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntegrityInventory {
    pub observed_at: String,
    pub bucket_incarnation: String,
    pub items: Vec<IntegrityInventoryItem>,
    pub is_truncated: bool,
    pub next_key_marker: Option<String>,
    pub next_version_marker: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntegrityInventoryItem {
    pub key: String,
    pub version_id: String,
    pub size: i64,
    pub protection: String,
    pub audit_unsupported: Option<String>,
    pub source_fingerprint: String,
    pub observation_error: Option<String>,
    #[serde(flatten)]
    pub details: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default)]
pub struct IntegrityInventoryQuery {
    pub prefix: String,
    pub key_marker: Option<String>,
    pub version_marker: Option<String>,
    pub limit: u16,
}

#[async_trait]
pub trait RecoveryApi: Send + Sync {
    async fn mrf_summary(&self, cursor: Option<&str>) -> Result<MrfObservation>;
    async fn mrf_list(&self, cursor: Option<&str>, limit: u16) -> Result<MrfObservation>;
    async fn mrf_get(&self, node: &str, id: &str) -> Result<MrfObservation>;
    async fn mrf_action(&self, action: &MrfAction) -> Result<MrfObservation>;
    async fn integrity_readiness(&self) -> Result<Value>;
    async fn integrity_inventory(
        &self,
        bucket: &str,
        query: &IntegrityInventoryQuery,
    ) -> Result<IntegrityInventory>;
    async fn integrity_create(
        &self,
        bucket: &str,
        request: &IntegrityJobRequest,
    ) -> Result<IntegrityJob>;
    async fn integrity_job(&self, bucket: &str, id: &str) -> Result<IntegrityJob>;
    async fn integrity_control(
        &self,
        bucket: &str,
        id: &str,
        operation: &str,
    ) -> Result<IntegrityJob>;
}

/// Rejects malformed selectors before they can become URL path components.
pub fn validate_recovery_uuid(value: &str) -> Result<()> {
    let valid = value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
        && value.bytes().any(|byte| byte != b'0' && byte != b'-');
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidPath("expected a non-nil UUID".into()))
    }
}

/// Accepts ordinary hex input while preserving the server's canonical Base64 wire contract.
pub fn normalize_integrity_sha256(value: &str) -> Result<String> {
    let raw = if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        (0..64)
            .step_by(2)
            .map(|index| {
                u8::from_str_radix(&value[index..index + 2], 16)
                    .map_err(|_| Error::InvalidPath("invalid SHA-256".into()))
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        let raw = STANDARD
            .decode(value)
            .map_err(|_| Error::InvalidPath("SHA-256 must be hex or canonical Base64".into()))?;
        if STANDARD.encode(&raw) != value {
            return Err(Error::InvalidPath(
                "SHA-256 must be canonical Base64".into(),
            ));
        }
        raw
    };
    if raw.len() != 32 {
        return Err(Error::InvalidPath("SHA-256 must contain 32 bytes".into()));
    }
    Ok(STANDARD.encode(raw))
}

impl IntegrityJobRequest {
    pub fn validate_and_normalize(&mut self) -> Result<()> {
        if !(1..=64).contains(&self.items.len())
            || !(64 * 1024..=1024 * 1024 * 1024).contains(&self.bytes_per_second)
            || !(1..=5 * 1024 * 1024 * 1024).contains(&self.max_object_bytes)
            || !["audit", "migrate"].contains(&self.mode.as_str())
        {
            return Err(Error::InvalidPath(
                "job requires 1..64 items, 64 KiB/s..1 GiB/s, and a 1 byte..5 GiB object budget"
                    .into(),
            ));
        }
        let sources: HashSet<_> = self.items.iter().map(|item| item.key.clone()).collect();
        let mut targets = HashSet::new();
        for item in &mut self.items {
            validate_key(&item.key)?;
            if let Some(version) = &item.version_id
                && version != "null"
            {
                validate_recovery_uuid(version)?;
            }
            if let Some(hash) = &item.expected_sha256 {
                item.expected_sha256 = Some(normalize_integrity_sha256(hash)?);
            }
            match (self.mode.as_str(), &item.target_key) {
                ("audit", None) => {}
                ("migrate", Some(target)) => {
                    validate_key(target)?;
                    if item.expected_sha256.is_none()
                        || sources.contains(target)
                        || !targets.insert(target.clone())
                    {
                        return Err(Error::InvalidPath(
                            "migration requires explicit digests and distinct new target keys"
                                .into(),
                        ));
                    }
                }
                _ => {
                    return Err(Error::InvalidPath(
                        "only migration accepts a target_key".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 1024 || key.ends_with('/') || key.contains('\0') {
        Err(Error::InvalidPath("invalid integrity object key".into()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_input_converts_hex_and_rejects_wrong_size() {
        assert_eq!(
            normalize_integrity_sha256(&"ab".repeat(32)).unwrap(),
            STANDARD.encode([0xab; 32])
        );
        assert!(normalize_integrity_sha256(&"ab".repeat(31)).is_err());
        assert!(normalize_integrity_sha256("not a digest").is_err());
    }

    #[test]
    fn current_version_and_null_slot_remain_distinct() {
        let mut request = IntegrityJobRequest {
            mode: "audit".into(),
            items: vec![IntegrityItem {
                key: "key".into(),
                version_id: Some("null".into()),
                expected_sha256: Some("ab".repeat(32)),
                target_key: None,
            }],
            bytes_per_second: 65536,
            max_object_bytes: 1024,
        };
        request.validate_and_normalize().unwrap();
        assert_eq!(
            serde_json::to_value(&request).unwrap()["items"][0]["version_id"],
            "null"
        );
        request.items[0].version_id = None;
        assert!(serde_json::to_value(&request).unwrap()["items"][0]["version_id"].is_null());
        request.mode = "migrate".into();
        request.items[0].target_key = Some("key".into());
        assert!(request.validate_and_normalize().is_err());
    }
}
