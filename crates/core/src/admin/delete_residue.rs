//! Operator observations and conditional discard of historical delete evidence.

use crate::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DeleteResidueTarget {
    pub bucket: String,
    pub object: String,
    pub pool: usize,
    pub set: usize,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ResidueSource {
    pub pool: usize,
    pub set: usize,
    pub drive: usize,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ResidueSourceStatus {
    Scanned,
    Unavailable,
    Unreadable,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ResidueCandidate {
    pub object: String,
    pub transaction: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteResiduePage {
    pub bucket: String,
    pub incarnation: String,
    pub source: ResidueSource,
    pub source_status: ResidueSourceStatus,
    pub candidates: Vec<ResidueCandidate>,
    pub next_cursor: Option<String>,
    pub incomplete_sources: usize,
    pub complete: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteResidueInspection {
    #[serde(flatten)]
    pub target: DeleteResidueTarget,
    pub incarnation: String,
    pub complete: bool,
    pub reason: String,
    pub participants: Vec<ResidueParticipant>,
    pub transactions: Vec<ResidueTransaction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    pub reclaimed: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResidueParticipant {
    pub slot: usize,
    pub disk_id: Option<String>,
    pub discovery: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ResidueTransaction {
    pub id: String,
    pub copies: Vec<ResidueCopy>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResidueCopy {
    pub slot: usize,
    pub state: String,
    pub current_metadata_present: Option<bool>,
    pub backup_present: Option<bool>,
    pub journal_present: Option<bool>,
    pub fingerprint: Option<String>,
}

#[async_trait]
pub trait DeleteResidueApi: Send + Sync {
    async fn list(
        &self,
        bucket: &str,
        cursor: Option<&str>,
        limit: u16,
    ) -> Result<DeleteResiduePage>;
    async fn inspect(&self, target: &DeleteResidueTarget) -> Result<DeleteResidueInspection>;
    /// Send the user's exact inspection fingerprint once; never rediscover or retry implicitly.
    async fn discard(
        &self,
        target: &DeleteResidueTarget,
        expected_fingerprint: &str,
    ) -> Result<DeleteResidueInspection>;
}
