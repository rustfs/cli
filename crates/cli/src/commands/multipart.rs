//! Shared workflows for listing and aborting incomplete multipart uploads.

use std::collections::HashSet;
use std::future::Future;

use futures::{StreamExt as _, stream};
use rc_core::alias::RetryConfig;
use rc_core::{
    AbortMultipartUploadRequest, Error, MultipartUpload, MultipartUploadListOptions,
    MultipartUploadListResult, is_retryable_error, retry_with_backoff,
};
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::Formatter;

pub(super) const DEFAULT_ABORT_CONCURRENCY: usize = 8;
const MULTIPART_OUTPUT_SCHEMA_VERSION: u8 = 3;
const MULTIPART_OUTPUT_TYPE: &str = "multipart_uploads";

#[derive(Debug, Serialize)]
struct MultipartOutputEnvelope<T> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: T,
}

#[derive(Debug, Serialize)]
struct MultipartErrorEnvelope {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    error: MultipartOutputError,
}

#[derive(Debug, Serialize)]
struct MultipartListingData<'a> {
    items: &'a [MultipartUpload],
    pagination: MultipartPagination,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<MultipartListingSummary>,
}

#[derive(Debug, Serialize)]
struct MultipartPagination {
    truncated: bool,
    continuation_token: Option<String>,
}

#[derive(Debug, Serialize)]
struct MultipartListingSummary {
    total_uploads: usize,
}

#[derive(Debug, Serialize)]
struct MultipartCleanupData {
    operation: &'static str,
    dry_run: bool,
    results: Vec<MultipartCleanupOutputItem>,
    summary: MultipartCleanupSummary,
}

#[derive(Debug, Serialize)]
struct MultipartCleanupSummary {
    total: usize,
    succeeded: usize,
    failed: usize,
}

#[derive(Debug, Serialize)]
pub(super) struct MultipartCleanupOutputItem {
    target: String,
    upload: Option<MultipartUpload>,
    state: &'static str,
    error: Option<MultipartOutputError>,
}

impl MultipartCleanupOutputItem {
    pub(super) fn identity(&self) -> (&str, &str) {
        (
            self.target.as_str(),
            self.upload
                .as_ref()
                .map(|upload| upload.upload_id.as_str())
                .unwrap_or_default(),
        )
    }

    pub(super) fn succeeded(target: String, upload: MultipartUpload, dry_run: bool) -> Self {
        Self {
            target,
            upload: Some(upload),
            state: if dry_run { "would_abort" } else { "aborted" },
            error: None,
        }
    }

    pub(super) fn failed(
        target: String,
        upload: Option<MultipartUpload>,
        code: ExitCode,
        message: String,
        capability: &'static str,
    ) -> Self {
        Self {
            target,
            upload,
            state: "failed",
            error: Some(MultipartOutputError::from_exit_code(
                code, message, capability,
            )),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum MultipartOutputError {
    Standard(MultipartStandardError),
    Unsupported(MultipartUnsupportedError),
}

impl MultipartOutputError {
    fn from_exit_code(code: ExitCode, message: String, capability: &'static str) -> Self {
        if code == ExitCode::UnsupportedFeature {
            return Self::Unsupported(MultipartUnsupportedError {
                error_type: "unsupported_feature",
                message,
                retryable: false,
                capability,
                server: None,
                suggestion: Some(
                    "Use an exact object key, or upgrade after rustfs/backlog#1384 is fixed.",
                ),
            });
        }

        let (error_type, retryable, suggestion) = match code {
            ExitCode::UsageError => (
                "usage_error",
                false,
                Some("Review the command arguments and retry."),
            ),
            ExitCode::NetworkError => (
                "network_error",
                true,
                Some("Verify the endpoint and network connectivity, then retry."),
            ),
            ExitCode::AuthError => (
                "auth_error",
                false,
                Some("Verify the alias credentials and permissions, then retry."),
            ),
            ExitCode::NotFound => (
                "not_found",
                false,
                Some("Check the alias, bucket, and object key, then retry."),
            ),
            ExitCode::Conflict => (
                "conflict",
                false,
                Some("Review the target state and retry."),
            ),
            ExitCode::Interrupted => (
                "interrupted",
                true,
                Some("Rerun the cleanup; aborting an upload is idempotent."),
            ),
            ExitCode::Success | ExitCode::GeneralError | ExitCode::UnsupportedFeature => {
                ("general_error", false, None)
            }
        };
        Self::Standard(MultipartStandardError {
            error_type,
            message,
            retryable,
            suggestion,
        })
    }
}

#[derive(Debug, Serialize)]
struct MultipartStandardError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggestion: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct MultipartUnsupportedError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    capability: &'static str,
    server: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggestion: Option<&'static str>,
}

pub(super) fn output_multipart_listing(
    formatter: &Formatter,
    uploads: &[MultipartUpload],
    summarize: bool,
) {
    formatter.json(&MultipartOutputEnvelope {
        schema_version: MULTIPART_OUTPUT_SCHEMA_VERSION,
        output_type: MULTIPART_OUTPUT_TYPE,
        status: "success",
        data: MultipartListingData {
            items: uploads,
            pagination: MultipartPagination {
                truncated: false,
                continuation_token: None,
            },
            summary: summarize.then_some(MultipartListingSummary {
                total_uploads: uploads.len(),
            }),
        },
    });
}

pub(super) fn output_multipart_cleanup(
    formatter: &Formatter,
    dry_run: bool,
    results: Vec<MultipartCleanupOutputItem>,
    succeeded: usize,
    failed: usize,
) {
    formatter.json(&MultipartOutputEnvelope {
        schema_version: MULTIPART_OUTPUT_SCHEMA_VERSION,
        output_type: MULTIPART_OUTPUT_TYPE,
        status: if failed == 0 { "success" } else { "partial" },
        data: MultipartCleanupData {
            operation: "abort",
            dry_run,
            summary: MultipartCleanupSummary {
                total: results.len(),
                succeeded,
                failed,
            },
            results,
        },
    });
}

pub(super) fn emit_multipart_error(
    formatter: &Formatter,
    code: ExitCode,
    message: impl Into<String>,
    capability: &'static str,
) -> ExitCode {
    let message = message.into();
    if formatter.is_json() {
        formatter.json(&MultipartErrorEnvelope {
            schema_version: MULTIPART_OUTPUT_SCHEMA_VERSION,
            output_type: MULTIPART_OUTPUT_TYPE,
            status: "error",
            error: MultipartOutputError::from_exit_code(code, message, capability),
        });
    } else {
        formatter.error_with_code(code, &message);
    }
    code
}

pub(super) async fn collect_multipart_uploads<F, Fut>(
    mut options: MultipartUploadListOptions,
    mut fetch_page: F,
) -> Result<Vec<MultipartUpload>, Error>
where
    F: FnMut(MultipartUploadListOptions) -> Fut,
    Fut: Future<Output = Result<MultipartUploadListResult, Error>>,
{
    let mut uploads = Vec::new();
    let mut seen_markers = HashSet::new();
    seen_markers.insert((options.key_marker.clone(), options.upload_id_marker.clone()));
    loop {
        let current_key_marker = options.key_marker.clone();
        let current_upload_id_marker = options.upload_id_marker.clone();
        let page = fetch_page(options.clone()).await?;
        uploads.extend(page.uploads);

        if !page.truncated {
            break;
        }

        let (next_key_marker, next_upload_id_marker) = match (
            page.next_key_marker,
            page.next_upload_id_marker,
        ) {
            (Some(key_marker), Some(upload_id_marker)) => (key_marker, upload_id_marker),
            _ => {
                return Err(Error::Network(
                        "S3 returned a truncated multipart upload listing without a complete marker pair"
                            .to_string(),
                    ));
            }
        };
        if current_key_marker.as_deref() == Some(next_key_marker.as_str())
            && current_upload_id_marker.as_deref() == Some(next_upload_id_marker.as_str())
        {
            return Err(Error::Network(
                "S3 returned a truncated multipart upload listing without advancing its markers"
                    .to_string(),
            ));
        }
        let next_markers = (Some(next_key_marker), Some(next_upload_id_marker));
        if !seen_markers.insert(next_markers.clone()) {
            return Err(Error::Network(
                "S3 returned a multipart upload pagination marker cycle".to_string(),
            ));
        }
        options.key_marker = next_markers.0;
        options.upload_id_marker = next_markers.1;
    }

    sort_uploads(&mut uploads);
    uploads.dedup_by(|left, right| {
        left.bucket == right.bucket && left.key == right.key && left.upload_id == right.upload_id
    });
    Ok(uploads)
}

#[derive(Debug)]
pub(super) struct MultipartCleanupOptions {
    pub dry_run: bool,
    pub concurrency: usize,
    pub retry: RetryConfig,
}

impl MultipartCleanupOptions {
    pub(super) fn command_default(dry_run: bool) -> Self {
        Self {
            dry_run,
            concurrency: DEFAULT_ABORT_CONCURRENCY,
            retry: RetryConfig {
                max_attempts: 3,
                initial_backoff_ms: 100,
                max_backoff_ms: 1_000,
            },
        }
    }
}

#[derive(Debug)]
pub(super) struct MultipartCleanupFailure {
    pub upload: MultipartUpload,
    pub error: Error,
}

#[derive(Debug, Default)]
pub(super) struct MultipartCleanupResult {
    pub completed: Vec<MultipartUpload>,
    pub failed: Vec<MultipartCleanupFailure>,
}

pub(super) async fn cleanup_multipart_uploads<F, Fut>(
    mut uploads: Vec<MultipartUpload>,
    options: MultipartCleanupOptions,
    abort: F,
) -> MultipartCleanupResult
where
    F: Fn(AbortMultipartUploadRequest) -> Fut + Sync,
    Fut: Future<Output = Result<(), Error>>,
{
    sort_uploads(&mut uploads);
    if options.dry_run {
        return MultipartCleanupResult {
            completed: uploads,
            failed: Vec::new(),
        };
    }

    let concurrency = options.concurrency.max(1);
    let results = stream::iter(uploads.into_iter().map(|upload| {
        let retry = options.retry.clone();
        let abort = &abort;
        async move {
            let request = AbortMultipartUploadRequest {
                bucket: upload.bucket.clone(),
                key: upload.key.clone(),
                upload_id: upload.upload_id.clone(),
            };
            let result =
                retry_with_backoff(&retry, || abort(request.clone()), is_retryable_error).await;
            (upload, result)
        }
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await;

    let mut cleanup = MultipartCleanupResult::default();
    for (upload, result) in results {
        match result {
            Ok(()) => cleanup.completed.push(upload),
            Err(error) => cleanup
                .failed
                .push(MultipartCleanupFailure { upload, error }),
        }
    }
    sort_uploads(&mut cleanup.completed);
    cleanup
        .failed
        .sort_by(|left, right| upload_identity(&left.upload).cmp(&upload_identity(&right.upload)));
    cleanup
}

fn sort_uploads(uploads: &mut [MultipartUpload]) {
    uploads.sort_by(|left, right| upload_identity(left).cmp(&upload_identity(right)));
}

fn upload_identity(upload: &MultipartUpload) -> (&str, &str, &str) {
    (&upload.bucket, &upload.key, &upload.upload_id)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    fn upload(key: &str, upload_id: &str) -> MultipartUpload {
        MultipartUpload {
            bucket: "bucket".to_string(),
            key: key.to_string(),
            upload_id: upload_id.to_string(),
            initiated: None,
            size_bytes: None,
            storage_class: None,
            initiator: None,
            owner: None,
            checksum_algorithm: None,
            checksum_type: None,
        }
    }

    #[tokio::test]
    async fn empty_listing_returns_an_empty_collection() {
        let mut pages = VecDeque::from([MultipartUploadListResult {
            uploads: Vec::new(),
            common_prefixes: Vec::new(),
            truncated: false,
            next_key_marker: None,
            next_upload_id_marker: None,
        }]);

        let uploads = collect_multipart_uploads(MultipartUploadListOptions::default(), |_| {
            std::future::ready(Ok(pages.pop_front().expect("test page should exist")))
        })
        .await
        .expect("empty listing should succeed");

        assert!(uploads.is_empty());
    }

    #[tokio::test]
    async fn truncated_listing_requires_both_pagination_markers() {
        for (next_key_marker, next_upload_id_marker) in [
            (Some("key".to_string()), None),
            (None, Some("upload".to_string())),
        ] {
            let result = collect_multipart_uploads(MultipartUploadListOptions::default(), |_| {
                std::future::ready(Ok(MultipartUploadListResult {
                    uploads: vec![upload("key", "upload")],
                    common_prefixes: Vec::new(),
                    truncated: true,
                    next_key_marker: next_key_marker.clone(),
                    next_upload_id_marker: next_upload_id_marker.clone(),
                }))
            })
            .await;

            match result {
                Err(Error::Network(message)) => {
                    assert!(message.contains("complete marker pair"));
                }
                other => panic!("expected incomplete marker error, got {other:?}"),
            }
        }
    }

    #[test]
    fn listing_output_uses_the_v3_envelope() {
        let uploads = vec![upload("backup.tar", "upload-1")];
        let output = MultipartOutputEnvelope {
            schema_version: MULTIPART_OUTPUT_SCHEMA_VERSION,
            output_type: MULTIPART_OUTPUT_TYPE,
            status: "success",
            data: MultipartListingData {
                items: &uploads,
                pagination: MultipartPagination {
                    truncated: false,
                    continuation_token: None,
                },
                summary: Some(MultipartListingSummary { total_uploads: 1 }),
            },
        };

        let json = serde_json::to_value(output).expect("v3 listing should serialize");
        assert_eq!(json["schema_version"], 3);
        assert_eq!(json["type"], "multipart_uploads");
        assert_eq!(json["status"], "success");
        assert_eq!(json["data"]["items"][0]["upload_id"], "upload-1");
        assert_eq!(json["data"]["summary"]["total_uploads"], 1);
    }

    #[test]
    fn cleanup_output_preserves_success_and_failure_results() {
        let output = MultipartOutputEnvelope {
            schema_version: MULTIPART_OUTPUT_SCHEMA_VERSION,
            output_type: MULTIPART_OUTPUT_TYPE,
            status: "partial",
            data: MultipartCleanupData {
                operation: "abort",
                dry_run: false,
                results: vec![
                    MultipartCleanupOutputItem::succeeded(
                        "local/bucket/ok.bin".to_string(),
                        upload("ok.bin", "1"),
                        false,
                    ),
                    MultipartCleanupOutputItem::failed(
                        "local/bucket/denied.bin".to_string(),
                        Some(upload("denied.bin", "2")),
                        ExitCode::AuthError,
                        "Access denied".to_string(),
                        "abort_multipart_upload",
                    ),
                ],
                summary: MultipartCleanupSummary {
                    total: 2,
                    succeeded: 1,
                    failed: 1,
                },
            },
        };

        let json = serde_json::to_value(output).expect("v3 cleanup should serialize");
        assert_eq!(json["status"], "partial");
        assert_eq!(json["data"]["results"][0]["state"], "aborted");
        assert_eq!(json["data"]["results"][1]["state"], "failed");
        assert_eq!(json["data"]["results"][1]["error"]["type"], "auth_error");
    }

    #[tokio::test]
    async fn dry_run_returns_sorted_plan_without_calling_abort() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_for_abort = Arc::clone(&calls);
        let result = cleanup_multipart_uploads(
            vec![upload("z.bin", "2"), upload("a.bin", "1")],
            MultipartCleanupOptions {
                dry_run: true,
                concurrency: 2,
                retry: RetryConfig::default(),
            },
            move |_| {
                calls_for_abort.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::future::ready(Ok(()))
            },
        )
        .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(result.completed[0].key, "a.bin");
        assert_eq!(result.completed[1].key, "z.bin");
        assert!(result.failed.is_empty());
    }

    #[tokio::test]
    async fn cleanup_retries_transient_failures_and_preserves_per_upload_errors() {
        let attempts = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let attempts_for_abort = Arc::clone(&attempts);
        let result = cleanup_multipart_uploads(
            vec![upload("retry.bin", "1"), upload("denied.bin", "2")],
            MultipartCleanupOptions {
                dry_run: false,
                concurrency: 2,
                retry: RetryConfig {
                    max_attempts: 3,
                    initial_backoff_ms: 1,
                    max_backoff_ms: 2,
                },
            },
            move |request| {
                let mut attempts = attempts_for_abort
                    .lock()
                    .expect("attempt counter lock should not be poisoned");
                let count = attempts.entry(request.key.clone()).or_default();
                *count += 1;
                let result = if request.key == "retry.bin" && *count < 3 {
                    Err(Error::Network("503 Service Unavailable".to_string()))
                } else if request.key == "denied.bin" {
                    Err(Error::Auth("AccessDenied".to_string()))
                } else {
                    Ok(())
                };
                std::future::ready(result)
            },
        )
        .await;

        let attempts = attempts
            .lock()
            .expect("attempt counter lock should not be poisoned");
        assert_eq!(attempts.get("retry.bin"), Some(&3));
        assert_eq!(attempts.get("denied.bin"), Some(&1));
        assert_eq!(result.completed.len(), 1);
        assert_eq!(result.completed[0].key, "retry.bin");
        assert_eq!(result.failed.len(), 1);
        assert_eq!(result.failed[0].upload.key, "denied.bin");
        assert!(matches!(result.failed[0].error, Error::Auth(_)));
    }

    #[tokio::test]
    async fn cleanup_never_exceeds_the_configured_concurrency() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let active_for_abort = Arc::clone(&active);
        let maximum_for_abort = Arc::clone(&maximum);
        let uploads = (0..6)
            .map(|index| upload(&format!("{index}.bin"), &index.to_string()))
            .collect();

        let result = cleanup_multipart_uploads(
            uploads,
            MultipartCleanupOptions {
                dry_run: false,
                concurrency: 2,
                retry: RetryConfig::default(),
            },
            move |_| {
                let active = Arc::clone(&active_for_abort);
                let maximum = Arc::clone(&maximum_for_abort);
                async move {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(current, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .await;

        assert_eq!(result.completed.len(), 6);
        assert!(result.failed.is_empty());
        assert!(maximum.load(Ordering::SeqCst) > 1);
        assert!(maximum.load(Ordering::SeqCst) <= 2);
    }
}
