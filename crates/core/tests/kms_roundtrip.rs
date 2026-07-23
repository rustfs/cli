use async_trait::async_trait;
use rc_core::admin::{
    KmsDiagnosticStore, KmsRoundTripErrorClass, KmsRoundTripPhase, run_kms_round_trip,
};
use rc_core::{Error, RemotePath, Result};
use std::sync::Mutex;
use zeroize::Zeroizing;

#[derive(Clone, Copy)]
enum Scenario {
    Success,
    WriteFailure,
    ReadFailure,
    Mismatch,
    PermissionDenied,
    CleanupFailure,
}

struct FakeDiagnosticStore {
    scenario: Scenario,
    uploaded: Mutex<Vec<u8>>,
    object_key: Mutex<Option<String>>,
    cleanup_attempts: Mutex<u32>,
}

impl FakeDiagnosticStore {
    fn new(scenario: Scenario) -> Self {
        Self {
            scenario,
            uploaded: Mutex::new(Vec::new()),
            object_key: Mutex::new(None),
            cleanup_attempts: Mutex::new(0),
        }
    }

    fn cleanup_attempts(&self) -> u32 {
        *self.cleanup_attempts.lock().expect("cleanup counter lock")
    }
}

#[async_trait]
impl KmsDiagnosticStore for FakeDiagnosticStore {
    async fn put_kms_diagnostic_object(
        &self,
        path: &RemotePath,
        content: Zeroizing<Vec<u8>>,
        _key_id: &str,
    ) -> Result<()> {
        *self.object_key.lock().expect("object key lock") = Some(path.key.clone());
        if matches!(self.scenario, Scenario::WriteFailure) {
            return Err(Error::Network("server detail must not escape".to_string()));
        }
        if matches!(self.scenario, Scenario::PermissionDenied) {
            return Err(Error::Auth("credential detail must not escape".to_string()));
        }
        *self.uploaded.lock().expect("uploaded content lock") = content.to_vec();
        Ok(())
    }

    async fn get_kms_diagnostic_object(
        &self,
        path: &RemotePath,
        _max_bytes: usize,
    ) -> Result<Zeroizing<Vec<u8>>> {
        assert_eq!(
            self.object_key.lock().expect("object key lock").as_deref(),
            Some(path.key.as_str())
        );
        if matches!(self.scenario, Scenario::ReadFailure) {
            return Err(Error::Network("decrypt detail must not escape".to_string()));
        }
        if matches!(self.scenario, Scenario::Mismatch) {
            return Ok(Zeroizing::new(vec![0_u8; 8]));
        }
        Ok(Zeroizing::new(
            self.uploaded.lock().expect("uploaded content lock").clone(),
        ))
    }

    async fn delete_kms_diagnostic_object(&self, path: &RemotePath) -> Result<()> {
        assert_eq!(
            self.object_key.lock().expect("object key lock").as_deref(),
            Some(path.key.as_str())
        );
        *self.cleanup_attempts.lock().expect("cleanup counter lock") += 1;
        if matches!(self.scenario, Scenario::CleanupFailure) {
            Err(Error::Network("cleanup detail must not escape".to_string()))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn roundtrip_success_reports_only_safe_metadata_and_cleans_up() {
    let store = FakeDiagnosticStore::new(Scenario::Success);
    let report = run_kms_round_trip(&store, "diagnostic-bucket", "key-1")
        .await
        .expect("round-trip should pass");

    assert!(report.passed);
    assert!(report.cleanup_passed);
    assert_eq!(report.bucket, "diagnostic-bucket");
    assert_eq!(report.key_id, "key-1");
    assert_eq!(store.cleanup_attempts(), 1);
    let output = serde_json::to_string(&report).expect("report should serialize");
    assert!(!output.contains("object"));
    assert!(!output.contains("content"));
    assert!(!output.contains("digest"));
}

#[tokio::test]
async fn roundtrip_write_failure_still_attempts_cleanup() {
    let store = FakeDiagnosticStore::new(Scenario::WriteFailure);
    let error = run_kms_round_trip(&store, "diagnostic-bucket", "key-1")
        .await
        .expect_err("write should fail");

    assert_eq!(error.phase, KmsRoundTripPhase::Write);
    assert_eq!(error.class, KmsRoundTripErrorClass::Network);
    assert!(!error.cleanup_failed);
    assert_eq!(store.cleanup_attempts(), 1);
    assert!(!error.to_string().contains("server detail"));
}

#[tokio::test]
async fn roundtrip_read_failure_still_attempts_cleanup() {
    let store = FakeDiagnosticStore::new(Scenario::ReadFailure);
    let error = run_kms_round_trip(&store, "diagnostic-bucket", "key-1")
        .await
        .expect_err("read should fail");

    assert_eq!(error.phase, KmsRoundTripPhase::Read);
    assert_eq!(error.class, KmsRoundTripErrorClass::Network);
    assert!(!error.cleanup_failed);
    assert_eq!(store.cleanup_attempts(), 1);
    assert!(!error.to_string().contains("decrypt detail"));
}

#[tokio::test]
async fn roundtrip_mismatch_is_distinct_and_cleans_up() {
    let store = FakeDiagnosticStore::new(Scenario::Mismatch);
    let error = run_kms_round_trip(&store, "diagnostic-bucket", "key-1")
        .await
        .expect_err("mismatch should fail");

    assert_eq!(error.phase, KmsRoundTripPhase::Verify);
    assert_eq!(error.class, KmsRoundTripErrorClass::General);
    assert!(!error.cleanup_failed);
    assert_eq!(store.cleanup_attempts(), 1);
}

#[tokio::test]
async fn roundtrip_permission_denial_is_preserved_without_details() {
    let store = FakeDiagnosticStore::new(Scenario::PermissionDenied);
    let error = run_kms_round_trip(&store, "diagnostic-bucket", "key-1")
        .await
        .expect_err("permission denial should fail");

    assert_eq!(error.phase, KmsRoundTripPhase::Write);
    assert_eq!(error.class, KmsRoundTripErrorClass::Auth);
    assert_eq!(store.cleanup_attempts(), 1);
    assert!(!error.to_string().contains("credential detail"));
}

#[tokio::test]
async fn roundtrip_cleanup_failure_is_reported_separately() {
    let store = FakeDiagnosticStore::new(Scenario::CleanupFailure);
    let error = run_kms_round_trip(&store, "diagnostic-bucket", "key-1")
        .await
        .expect_err("cleanup should fail");

    assert_eq!(error.phase, KmsRoundTripPhase::Cleanup);
    assert_eq!(error.class, KmsRoundTripErrorClass::Network);
    assert!(error.cleanup_failed);
    assert_eq!(store.cleanup_attempts(), 1);
    assert!(!error.to_string().contains("cleanup detail"));
}
