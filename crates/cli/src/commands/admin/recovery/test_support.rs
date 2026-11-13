use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use rc_core::Result;
use rc_core::admin::recovery::*;
use serde_json::Value;

pub const ENDPOINT: &str = "http://localhost:9000/";
pub const ID: &str = "11111111-1111-4111-8111-111111111111";
pub const INCARNATION: &str = "22222222-2222-4222-8222-222222222222";
pub const REQUEST_ID: &str = "33333333-3333-4333-8333-333333333333";
pub const NODE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[derive(Debug, PartialEq)]
pub enum Call {
    Summary(Option<String>),
    List(Option<String>, u16),
    Get(String, String),
    Action(serde_json::Value),
    Readiness,
    Inventory(String),
    Create(String, IntegrityJobRequest),
    Job(String, String),
    Control(String, String, String),
}

#[derive(Default)]
pub struct MockApi {
    pub calls: Mutex<Vec<Call>>,
    pub observations: Mutex<VecDeque<Result<MrfObservation>>>,
    pub readiness: Mutex<VecDeque<Result<Value>>>,
    pub inventories: Mutex<VecDeque<Result<IntegrityInventory>>>,
    pub creates: Mutex<VecDeque<Result<IntegrityJob>>>,
    pub jobs: Mutex<VecDeque<Result<IntegrityJob>>>,
    pub controls: Mutex<VecDeque<Result<IntegrityJob>>>,
    pub create_receipt_collision: Option<std::path::PathBuf>,
}

fn pop<T>(queue: &Mutex<VecDeque<Result<T>>>) -> Result<T> {
    queue
        .lock()
        .expect("mock reply queue")
        .pop_front()
        .expect("unexpected API call")
}

impl MockApi {
    fn record(&self, call: Call) {
        self.calls.lock().expect("mock call log").push(call);
    }
}

#[async_trait]
impl RecoveryApi for MockApi {
    async fn mrf_summary(&self, cursor: Option<&str>) -> Result<MrfObservation> {
        self.record(Call::Summary(cursor.map(str::to_owned)));
        pop(&self.observations)
    }
    async fn mrf_list(&self, cursor: Option<&str>, limit: u16) -> Result<MrfObservation> {
        self.record(Call::List(cursor.map(str::to_owned), limit));
        pop(&self.observations)
    }
    async fn mrf_get(&self, node: &str, id: &str) -> Result<MrfObservation> {
        self.record(Call::Get(node.into(), id.into()));
        pop(&self.observations)
    }
    async fn mrf_action(&self, action: &MrfAction) -> Result<MrfObservation> {
        self.record(Call::Action(
            serde_json::to_value(action).expect("action JSON"),
        ));
        pop(&self.observations)
    }
    async fn integrity_readiness(&self) -> Result<Value> {
        self.record(Call::Readiness);
        pop(&self.readiness)
    }
    async fn integrity_inventory(
        &self,
        bucket: &str,
        _: &IntegrityInventoryQuery,
    ) -> Result<IntegrityInventory> {
        self.record(Call::Inventory(bucket.into()));
        pop(&self.inventories)
    }
    async fn integrity_create(
        &self,
        bucket: &str,
        request: &IntegrityJobRequest,
    ) -> Result<IntegrityJob> {
        self.record(Call::Create(bucket.into(), request.clone()));
        if let Some(path) = &self.create_receipt_collision {
            std::fs::write(path, b"another operation owns this receipt")
                .expect("simulate receipt race");
        }
        pop(&self.creates)
    }
    async fn integrity_job(&self, bucket: &str, id: &str) -> Result<IntegrityJob> {
        self.record(Call::Job(bucket.into(), id.into()));
        pop(&self.jobs)
    }
    async fn integrity_control(
        &self,
        bucket: &str,
        id: &str,
        operation: &str,
    ) -> Result<IntegrityJob> {
        self.record(Call::Control(bucket.into(), id.into(), operation.into()));
        pop(&self.controls)
    }
}

pub fn formatter() -> crate::output::Formatter {
    crate::output::Formatter::new(crate::output::OutputConfig {
        quiet: true,
        ..Default::default()
    })
}

pub fn observation(reply: MrfReply) -> MrfObservation {
    MrfObservation {
        contract_version: 1,
        topology_fingerprint: "b".repeat(64),
        observation_started_at_ms: 10,
        observation_finished_at_ms: 20,
        expected_nodes: 1,
        page_complete: true,
        nodes: vec![MrfNodeObservation {
            node_id: NODE.into(),
            host: "node-a".into(),
            state: "ok".into(),
            reply: Some(reply),
        }],
        next_cursor: None,
    }
}

pub fn row() -> MrfResponsibility {
    serde_json::from_value(serde_json::json!({
        "responsibilityId": ID, "bucket": "bucket", "object": "object", "status": "held_unverified_legacy",
        "sourceBucketIncarnationId": INCARNATION, "bucketIncarnationId": INCARNATION,
        "versionId": null, "accepted": null
    })).expect("responsibility fixture")
}

pub fn inventory() -> IntegrityInventory {
    serde_json::from_value(serde_json::json!({
        "observed_at": "2026-11-13T00:00:00Z", "bucket_incarnation": INCARNATION,
        "items": [{"key":"object", "version_id":"null", "size":128, "protection":"legacy",
            "audit_unsupported":null, "source_fingerprint":"observed-source", "observation_error":null}],
        "is_truncated":false, "next_key_marker":null, "next_version_marker":null
    })).expect("inventory fixture")
}

pub fn request(mode: &str) -> IntegrityJobRequest {
    IntegrityJobRequest {
        mode: mode.into(),
        items: vec![IntegrityItem {
            key: "object".into(),
            version_id: Some("null".into()),
            expected_sha256: Some("q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6s=".into()),
            target_key: (mode == "migrate").then(|| "protected-copy".into()),
        }],
        bytes_per_second: 65536,
        max_object_bytes: 1024,
    }
}

pub fn job(mode: &str, state: &str, item_state: &str) -> IntegrityJob {
    IntegrityJob {
        format_version: 1,
        id: ID.into(),
        bucket: "bucket".into(),
        bucket_incarnation: INCARNATION.into(),
        request: request(mode),
        state: state.into(),
        revision: 1,
        results: vec![IntegrityItemResult {
            state: item_state.into(),
            source_fingerprint: Some("executed-source".into()),
            expected_sha256: request(mode).items[0].expected_sha256.clone(),
            evidence_source: Some("administrator_supplied_sha256".into()),
            source_size: Some(128),
            target_version_id: None,
            detail: None,
        }],
        last_error: None,
    }
}
