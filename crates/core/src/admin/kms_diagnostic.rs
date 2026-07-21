//! Non-exporting SSE-KMS object round-trip diagnostics.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use zeroize::{Zeroize, Zeroizing};

use crate::{Error, RemotePath, Result};

/// Fixed upper bound for internally generated diagnostic plaintext.
pub const KMS_DIAGNOSTIC_CONTENT_BYTES: usize = 4096;

/// Adapter boundary for the three sensitive object operations used by the diagnostic.
#[async_trait]
pub trait KmsDiagnosticStore: Send + Sync {
    /// Upload bounded plaintext with explicit SSE-KMS encryption.
    async fn put_kms_diagnostic_object(
        &self,
        path: &RemotePath,
        content: Zeroizing<Vec<u8>>,
        key_id: &str,
    ) -> Result<()>;

    /// Read and decrypt bounded diagnostic plaintext.
    async fn get_kms_diagnostic_object(
        &self,
        path: &RemotePath,
        max_bytes: usize,
    ) -> Result<Zeroizing<Vec<u8>>>;

    /// Permanently remove the temporary object, including from versioned buckets.
    async fn delete_kms_diagnostic_object(&self, path: &RemotePath) -> Result<()>;
}

/// Safe phase identifier for a failed diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KmsRoundTripPhase {
    Write,
    Read,
    Verify,
    Cleanup,
}

/// Stable error class retained after adapter diagnostics are discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KmsRoundTripErrorClass {
    Auth,
    NotFound,
    Network,
    Conflict,
    General,
}

/// Safe timing metadata for a completed diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KmsRoundTripTimings {
    pub write_ms: u64,
    pub read_ms: u64,
    pub cleanup_ms: u64,
    pub total_ms: u64,
}

/// Successful diagnostic result. Temporary names and content are intentionally absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KmsRoundTripReport {
    pub bucket: String,
    pub key_id: String,
    pub passed: bool,
    pub cleanup_passed: bool,
    pub timings: KmsRoundTripTimings,
}

/// Sanitized failure that preserves exit-code classification and cleanup state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KmsRoundTripError {
    pub phase: KmsRoundTripPhase,
    pub class: KmsRoundTripErrorClass,
    pub cleanup_failed: bool,
}

impl KmsRoundTripError {
    /// Convert the safe failure into the shared CLI error taxonomy.
    pub fn into_core_error(self) -> Error {
        let message = self.to_string();
        match self.class {
            KmsRoundTripErrorClass::Auth => Error::Auth(message),
            KmsRoundTripErrorClass::NotFound => Error::NotFound(message),
            KmsRoundTripErrorClass::Network => Error::Network(message),
            KmsRoundTripErrorClass::Conflict => Error::Conflict(message),
            KmsRoundTripErrorClass::General => Error::General(message),
        }
    }

    fn safe_message(self) -> &'static str {
        match (self.phase, self.cleanup_failed) {
            (KmsRoundTripPhase::Write, true) => {
                "KMS round-trip SSE-KMS write failed; temporary object cleanup also failed"
            }
            (KmsRoundTripPhase::Read, true) => {
                "KMS round-trip read/decrypt failed; temporary object cleanup also failed"
            }
            (KmsRoundTripPhase::Verify, true) => {
                "KMS round-trip verification mismatched; temporary object cleanup also failed"
            }
            (KmsRoundTripPhase::Write, false) => "KMS round-trip SSE-KMS write failed",
            (KmsRoundTripPhase::Read, false) => "KMS round-trip read/decrypt failed",
            (KmsRoundTripPhase::Verify, false) => "KMS round-trip verification mismatched",
            (KmsRoundTripPhase::Cleanup, _) => {
                "KMS round-trip verification passed, but temporary object cleanup failed"
            }
        }
    }
}

impl std::fmt::Display for KmsRoundTripError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.safe_message())
    }
}

impl std::error::Error for KmsRoundTripError {}

/// Run one bounded SSE-KMS write/read/verify/delete diagnostic.
pub async fn run_kms_round_trip(
    store: &dyn KmsDiagnosticStore,
    bucket: &str,
    key_id: &str,
) -> std::result::Result<KmsRoundTripReport, KmsRoundTripError> {
    let mut content = Zeroizing::new(vec![0_u8; KMS_DIAGNOSTIC_CONTENT_BYTES]);
    getrandom::fill(content.as_mut_slice()).map_err(|_| KmsRoundTripError {
        phase: KmsRoundTripPhase::Write,
        class: KmsRoundTripErrorClass::General,
        cleanup_failed: false,
    })?;

    let mut object_id = [0_u8; 16];
    getrandom::fill(&mut object_id).map_err(|_| KmsRoundTripError {
        phase: KmsRoundTripPhase::Write,
        class: KmsRoundTripErrorClass::General,
        cleanup_failed: false,
    })?;
    let path = RemotePath::new(
        "",
        bucket,
        format!(
            ".rustfs-kms-diagnostic/{:032x}",
            u128::from_be_bytes(object_id)
        ),
    );

    let total_started = Instant::now();
    let write_started = Instant::now();
    let write_result = store
        .put_kms_diagnostic_object(&path, Zeroizing::new(content.to_vec()), key_id)
        .await;
    let write_ms = duration_ms(write_started.elapsed());

    let read_started = Instant::now();
    let primary_failure = match write_result {
        Err(error) => Some((KmsRoundTripPhase::Write, classify_and_discard_error(error))),
        Ok(()) => match store
            .get_kms_diagnostic_object(&path, KMS_DIAGNOSTIC_CONTENT_BYTES)
            .await
        {
            Err(error) => Some((KmsRoundTripPhase::Read, classify_and_discard_error(error))),
            Ok(actual) if actual.as_slice() != content.as_slice() => {
                Some((KmsRoundTripPhase::Verify, KmsRoundTripErrorClass::General))
            }
            Ok(_) => None,
        },
    };
    let read_ms = duration_ms(read_started.elapsed());

    let cleanup_started = Instant::now();
    let cleanup_failure = store
        .delete_kms_diagnostic_object(&path)
        .await
        .err()
        .map(classify_and_discard_error);
    let cleanup_ms = duration_ms(cleanup_started.elapsed());

    match (primary_failure, cleanup_failure) {
        (Some((phase, class)), cleanup) => Err(KmsRoundTripError {
            phase,
            class,
            cleanup_failed: cleanup.is_some(),
        }),
        (None, Some(class)) => Err(KmsRoundTripError {
            phase: KmsRoundTripPhase::Cleanup,
            class,
            cleanup_failed: true,
        }),
        (None, None) => Ok(KmsRoundTripReport {
            bucket: bucket.to_string(),
            key_id: key_id.to_string(),
            passed: true,
            cleanup_passed: true,
            timings: KmsRoundTripTimings {
                write_ms,
                read_ms,
                cleanup_ms,
                total_ms: duration_ms(total_started.elapsed()),
            },
        }),
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn classify_and_discard_error(mut error: Error) -> KmsRoundTripErrorClass {
    match &mut error {
        Error::Auth(message) => {
            message.zeroize();
            KmsRoundTripErrorClass::Auth
        }
        Error::NotFound(message) | Error::AliasNotFound(message) => {
            message.zeroize();
            KmsRoundTripErrorClass::NotFound
        }
        Error::Network(message) => {
            message.zeroize();
            KmsRoundTripErrorClass::Network
        }
        Error::Conflict(message) | Error::AliasExists(message) => {
            message.zeroize();
            KmsRoundTripErrorClass::Conflict
        }
        Error::Config(message)
        | Error::InvalidPath(message)
        | Error::UnsupportedFeature(message)
        | Error::General(message) => {
            message.zeroize();
            KmsRoundTripErrorClass::General
        }
        Error::Io(_)
        | Error::TomlParse(_)
        | Error::TomlSerialize(_)
        | Error::Json(_)
        | Error::InvalidUrl(_) => KmsRoundTripErrorClass::General,
    }
}
