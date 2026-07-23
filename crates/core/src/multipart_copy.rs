//! Pure planning and request types for multipart server-side copies.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{Error, ObjectInfo, Result};
use tokio::sync::Notify;

/// Largest object that can be copied with a single S3 CopyObject request.
pub const S3_SINGLE_COPY_MAX_SIZE: u64 = 5 * 1024 * 1024 * 1024;

/// Smallest non-final part accepted by S3 multipart uploads.
pub const S3_MULTIPART_COPY_MIN_PART_SIZE: u64 = 5 * 1024 * 1024;

/// Largest part accepted by S3 multipart uploads.
pub const S3_MULTIPART_COPY_MAX_PART_SIZE: u64 = 5 * 1024 * 1024 * 1024;

/// Largest number of parts accepted by S3 multipart uploads.
pub const S3_MULTIPART_COPY_MAX_PARTS: usize = 10_000;

/// Largest object accepted by S3.
pub const S3_MAX_OBJECT_SIZE: u64 = 5 * 1024 * 1024 * 1024 * 1024;

/// Default multipart copy part size.
pub const DEFAULT_MULTIPART_COPY_PART_SIZE: u64 = 64 * 1024 * 1024;

/// Cooperative cancellation control for multipart server-side copies.
///
/// Callers must signal this control rather than dropping the copy future when
/// they need the backend to await multipart upload cleanup.
#[derive(Debug, Clone, Default)]
pub struct MultipartCopyCancellation {
    inner: Arc<MultipartCopyCancellationInner>,
}

#[derive(Debug, Default)]
struct MultipartCopyCancellationInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl MultipartCopyCancellation {
    /// Create a cancellation control in the active state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cooperative cancellation.
    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }

    /// Return whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// Wait until cancellation is requested without losing an early signal.
    pub async fn cancelled(&self) {
        let notified = self.inner.notify.notified();
        tokio::pin!(notified);
        // Register before reading the flag so cancellation cannot occur in the
        // gap between the state check and awaiting the notification.
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// A byte range assigned to one UploadPartCopy request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartCopyPart {
    /// One-based S3 part number.
    pub part_number: i32,
    /// Inclusive first byte offset.
    pub start: u64,
    /// Inclusive last byte offset.
    pub end_inclusive: u64,
    /// Number of logical bytes in the part.
    pub size: u64,
}

/// A validated multipart server-side copy plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartCopyPlan {
    /// Total logical source size.
    pub object_size: u64,
    /// Planned size of every non-final part.
    pub part_size: u64,
    /// Ordered, gap-free byte ranges.
    pub parts: Vec<MultipartCopyPart>,
}

impl MultipartCopyPlan {
    /// Build a plan that satisfies S3 part size and count limits.
    pub fn new(object_size: u64, preferred_part_size: Option<u64>) -> Result<Self> {
        if object_size == 0 {
            return Err(Error::InvalidPath(
                "Multipart copy source size must be greater than zero".to_string(),
            ));
        }
        if object_size > S3_MAX_OBJECT_SIZE {
            return Err(Error::InvalidPath(format!(
                "Multipart copy source size exceeds the S3 object limit of {S3_MAX_OBJECT_SIZE} bytes"
            )));
        }

        let preferred_part_size = preferred_part_size.unwrap_or(DEFAULT_MULTIPART_COPY_PART_SIZE);
        if !(S3_MULTIPART_COPY_MIN_PART_SIZE..=S3_MULTIPART_COPY_MAX_PART_SIZE)
            .contains(&preferred_part_size)
        {
            return Err(Error::InvalidPath(format!(
                "Multipart copy part size must be between {S3_MULTIPART_COPY_MIN_PART_SIZE} and {S3_MULTIPART_COPY_MAX_PART_SIZE} bytes"
            )));
        }

        // Raising a caller's preferred size is necessary when its requested size
        // would exceed S3's 10,000-part ceiling.
        let minimum_size_for_part_limit = object_size.div_ceil(S3_MULTIPART_COPY_MAX_PARTS as u64);
        let part_size = preferred_part_size
            .max(minimum_size_for_part_limit)
            .max(S3_MULTIPART_COPY_MIN_PART_SIZE);
        if part_size > S3_MULTIPART_COPY_MAX_PART_SIZE {
            return Err(Error::InvalidPath(format!(
                "Multipart copy requires a part larger than the S3 limit of {S3_MULTIPART_COPY_MAX_PART_SIZE} bytes"
            )));
        }

        let part_count = object_size.div_ceil(part_size);
        if part_count > S3_MULTIPART_COPY_MAX_PARTS as u64 {
            return Err(Error::InvalidPath(format!(
                "Multipart copy requires {part_count} parts, exceeding the S3 limit of {S3_MULTIPART_COPY_MAX_PARTS}"
            )));
        }

        let mut parts = Vec::with_capacity(part_count as usize);
        let mut start = 0;
        for part_index in 0..part_count {
            let end_exclusive = (start + part_size).min(object_size);
            parts.push(MultipartCopyPart {
                part_number: i32::try_from(part_index + 1)
                    .expect("S3 multipart copy plans contain at most 10,000 parts"),
                start,
                end_inclusive: end_exclusive - 1,
                size: end_exclusive - start,
            });
            start = end_exclusive;
        }

        Ok(Self {
            object_size,
            part_size,
            parts,
        })
    }
}

/// Return whether S3 requires multipart server-side copy for this object size.
pub const fn requires_multipart_copy(object_size: u64) -> bool {
    object_size > S3_SINGLE_COPY_MAX_SIZE
}

/// Inputs required for a consistent multipart server-side copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartCopyOptions {
    /// Logical source size used to build part ranges.
    pub source_size: u64,
    /// ETag sent as `x-amz-copy-source-if-match` on every part.
    pub source_etag: String,
    /// Exact historical source version, when selected.
    pub source_version_id: Option<String>,
    /// Optional preferred part size.
    pub preferred_part_size: Option<u64>,
    /// Destination content type inherited from the source.
    pub content_type: Option<String>,
    /// Destination user metadata inherited from the source.
    pub metadata: HashMap<String, String>,
}

impl MultipartCopyOptions {
    /// Build options with source identity and safe defaults.
    pub fn new(source_size: u64, source_etag: impl Into<String>) -> Result<Self> {
        let source_etag = source_etag.into();
        if source_etag.is_empty() {
            return Err(Error::InvalidPath(
                "Multipart copy source ETag cannot be empty".to_string(),
            ));
        }
        Ok(Self {
            source_size,
            source_etag,
            source_version_id: None,
            preferred_part_size: None,
            content_type: None,
            metadata: HashMap::new(),
        })
    }

    /// Validate the request and return its pure copy plan.
    pub fn plan(&self) -> Result<MultipartCopyPlan> {
        if self.source_etag.is_empty() {
            return Err(Error::InvalidPath(
                "Multipart copy source ETag cannot be empty".to_string(),
            ));
        }
        if self.source_version_id.as_deref().is_some_and(str::is_empty) {
            return Err(Error::InvalidPath(
                "Source version ID cannot be empty".to_string(),
            ));
        }
        MultipartCopyPlan::new(self.source_size, self.preferred_part_size)
    }
}

/// Successful multipart copy details.
#[derive(Debug, Clone)]
pub struct MultipartCopyResult {
    /// Destination object metadata.
    pub object: ObjectInfo,
    /// Service upload identifier used for the completed operation.
    pub upload_id: String,
    /// Number of successfully completed parts.
    pub part_count: usize,
    /// Logical bytes copied.
    pub bytes_copied: u64,
}

/// Callback invoked with cumulative successfully copied logical bytes.
pub type MultipartCopyProgress<'a> = dyn Fn(u64) + Send + Sync + 'a;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_threshold_is_strictly_greater_than_five_gibibytes() {
        assert!(!requires_multipart_copy(S3_SINGLE_COPY_MAX_SIZE));
        assert!(requires_multipart_copy(S3_SINGLE_COPY_MAX_SIZE + 1));
    }

    #[tokio::test]
    async fn cancellation_control_remembers_early_and_late_signals() {
        let early = MultipartCopyCancellation::new();
        early.cancel();
        early.cancel();
        early.cancelled().await;
        assert!(early.is_cancelled());

        let late = MultipartCopyCancellation::new();
        let waiter = late.clone();
        let task = tokio::spawn(async move {
            waiter.cancelled().await;
        });
        late.cancel();
        task.await.expect("cancellation waiter");
    }

    #[tokio::test]
    async fn cancellation_control_handles_registration_races() {
        for _ in 0..1_000 {
            let cancellation = MultipartCopyCancellation::new();
            let waiter = cancellation.clone();
            let task = tokio::spawn(async move {
                waiter.cancelled().await;
            });
            tokio::task::yield_now().await;
            cancellation.cancel();
            tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .expect("cancellation waiter must not lose notification")
                .expect("cancellation waiter task");
        }
    }

    #[test]
    fn plan_uses_inclusive_gap_free_ranges() {
        let object_size = S3_MULTIPART_COPY_MIN_PART_SIZE * 2 + 1;
        let plan = MultipartCopyPlan::new(object_size, Some(S3_MULTIPART_COPY_MIN_PART_SIZE))
            .expect("build multipart copy plan");

        assert_eq!(plan.parts.len(), 3);
        assert_eq!(
            plan.parts,
            vec![
                MultipartCopyPart {
                    part_number: 1,
                    start: 0,
                    end_inclusive: S3_MULTIPART_COPY_MIN_PART_SIZE - 1,
                    size: S3_MULTIPART_COPY_MIN_PART_SIZE,
                },
                MultipartCopyPart {
                    part_number: 2,
                    start: S3_MULTIPART_COPY_MIN_PART_SIZE,
                    end_inclusive: S3_MULTIPART_COPY_MIN_PART_SIZE * 2 - 1,
                    size: S3_MULTIPART_COPY_MIN_PART_SIZE,
                },
                MultipartCopyPart {
                    part_number: 3,
                    start: S3_MULTIPART_COPY_MIN_PART_SIZE * 2,
                    end_inclusive: object_size - 1,
                    size: 1,
                },
            ]
        );
        assert_eq!(
            plan.parts.iter().map(|part| part.size).sum::<u64>(),
            object_size
        );
    }

    #[test]
    fn plan_increases_part_size_to_stay_within_ten_thousand_parts() {
        let object_size =
            S3_MULTIPART_COPY_MIN_PART_SIZE * (S3_MULTIPART_COPY_MAX_PARTS as u64 + 1);
        let plan = MultipartCopyPlan::new(object_size, Some(S3_MULTIPART_COPY_MIN_PART_SIZE))
            .expect("adjust undersized preferred part");

        assert_eq!(plan.parts.len(), S3_MULTIPART_COPY_MAX_PARTS);
        assert_eq!(plan.part_size, object_size.div_ceil(10_000));
        assert_eq!(
            plan.parts.last().map(|part| part.end_inclusive),
            Some(object_size - 1)
        );
    }

    #[test]
    fn plan_accepts_exact_s3_boundaries() {
        let exact_max_parts_size =
            S3_MULTIPART_COPY_MIN_PART_SIZE * S3_MULTIPART_COPY_MAX_PARTS as u64;
        let plan =
            MultipartCopyPlan::new(exact_max_parts_size, Some(S3_MULTIPART_COPY_MIN_PART_SIZE))
                .expect("accept exactly ten thousand parts");
        assert_eq!(plan.parts.len(), S3_MULTIPART_COPY_MAX_PARTS);

        let max_object = MultipartCopyPlan::new(S3_MAX_OBJECT_SIZE, None)
            .expect("accept maximum S3 object size");
        assert!(max_object.parts.len() <= S3_MULTIPART_COPY_MAX_PARTS);
        assert!(max_object.part_size <= S3_MULTIPART_COPY_MAX_PART_SIZE);
    }

    #[test]
    fn plan_rejects_zero_oversized_objects_and_invalid_part_sizes() {
        for (object_size, part_size) in [
            (0, None),
            (S3_MAX_OBJECT_SIZE + 1, None),
            (1, Some(S3_MULTIPART_COPY_MIN_PART_SIZE - 1)),
            (1, Some(S3_MULTIPART_COPY_MAX_PART_SIZE + 1)),
        ] {
            assert!(
                MultipartCopyPlan::new(object_size, part_size).is_err(),
                "object_size={object_size}, part_size={part_size:?}"
            );
        }
    }

    #[test]
    fn options_reject_empty_source_identity() {
        assert!(MultipartCopyOptions::new(1, "").is_err());

        let mut options =
            MultipartCopyOptions::new(1, "etag").expect("valid multipart copy options");
        options.source_etag.clear();
        assert!(options.plan().is_err());

        options.source_etag = "etag".to_string();
        options.source_version_id = Some(String::new());
        assert!(options.plan().is_err());
    }
}
