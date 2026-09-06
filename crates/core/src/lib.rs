//! rc-core: Core library for the rc S3 CLI client
//!
//! This crate provides the core functionality for the rc CLI, including:
//! - Configuration management
//! - Alias management
//! - Path parsing and resolution
//! - ObjectStore trait for S3 operations
//!
//! This crate is designed to be independent of any specific S3 SDK,
//! allowing for easy testing and potential future support for other backends.

pub mod admin;
pub mod alias;
pub mod config;
pub mod cors;
pub mod encryption;
pub mod error;
pub mod lifecycle;
pub mod multipart_copy;
pub mod object_key;
pub mod object_lock;
pub mod ops;
pub mod path;
pub mod replication;
pub mod retry;
pub mod select;
pub mod traits;
pub mod transfer;
pub mod transfer_options;
pub mod undo;
pub mod watch;

pub use alias::{
    Alias, AliasManager, RequestHeader, global_request_headers, set_global_request_headers,
    validate_alias_endpoint,
};
pub use config::{Config, ConfigManager};
pub use cors::{CorsConfiguration, CorsRule};
pub use encryption::{BucketEncryption, ObjectEncryptionRequest};
pub use error::{Error, MultipartAbortStatus, Result};
pub use lifecycle::{
    LifecycleConfiguration, LifecycleDelMarkerExpiration, LifecycleExpiration, LifecycleRule,
    LifecycleRuleStatus, LifecycleTransition, NoncurrentVersionExpiration,
    NoncurrentVersionTransition,
};
pub use multipart_copy::{
    DEFAULT_MULTIPART_COPY_PART_SIZE, MultipartCopyCancellation, MultipartCopyOptions,
    MultipartCopyPart, MultipartCopyPlan, MultipartCopyProgress, MultipartCopyResult,
    S3_MAX_OBJECT_SIZE, S3_MULTIPART_COPY_MAX_PART_SIZE, S3_MULTIPART_COPY_MAX_PARTS,
    S3_MULTIPART_COPY_MIN_PART_SIZE, S3_SINGLE_COPY_MAX_SIZE, requires_multipart_copy,
};
pub use object_key::{ObjectKeyPolicy, normalize_relative_key, relative_local_path_from_key};
pub use object_lock::{
    BucketObjectLockConfiguration, DefaultRetention, LegalHoldStatus, ObjectLockOptions,
    ObjectRetention, RetentionDuration, RetentionDurationUnit, RetentionMode,
};
pub use ops::{
    HealthApi, HealthProbe, HealthReport, UsageBucket, UsageFailure, UsageReport, UsageScanApi,
    UsageScanRequest, UsageScope, UsageSnapshotApi, UsageSource,
};
pub use path::{ParsedPath, RemotePath, parse_object_path, parse_path};
pub use replication::{
    BucketTarget, BucketTargetCredentials, ReplicationCheckPhase, ReplicationCheckPhaseState,
    ReplicationCheckPhases, ReplicationCheckResult, ReplicationCheckStatus, ReplicationCheckTarget,
    ReplicationConfiguration, ReplicationDestination, ReplicationResyncStartOptions,
    ReplicationResyncStartResult, ReplicationResyncState, ReplicationResyncStatus,
    ReplicationResyncTargetStatus, ReplicationRule, ReplicationRuleStatus,
};
pub use retry::{RetryBuilder, is_retryable_error, retry_with_backoff};
pub use select::{
    SelectCompression, SelectCsvFileHeaderInfo, SelectCsvInputOptions, SelectCsvOutputOptions,
    SelectInputFormat, SelectJsonInputOptions, SelectJsonInputType, SelectJsonOutputOptions,
    SelectOptions, SelectOutputFormat, SelectQuoteFields, SelectScanRangeOptions,
    SelectSseCustomerOptions,
};
pub use traits::{
    AbortMultipartUploadRequest, BucketNotification, Capabilities, CopyObjectOptions,
    CreateBucketOptions, DeleteObjectFailure, DeleteObjectsResult, DeleteRequestOptions,
    DeletedObject, ListObjectVersionsOptions, ListOptions, ListResult, MultipartIdentity,
    MultipartUpload, MultipartUploadListOptions, MultipartUploadListResult, NotificationTarget,
    ObjectInfo, ObjectReadOptions, ObjectStore, ObjectVersion, ObjectVersionIdentifier,
    ObjectVersionListResult,
};
pub use transfer::{
    TransferCancellation, TransferCandidate, TransferControls, TransferExecutor, TransferOutcome,
    TransferOutcomeState, TransferPlan, TransferReport, TransferSelection, TransferSummary,
};
pub use transfer_options::{
    ChecksumAlgorithm, ChecksumRequest, MetadataDirective, ObjectAttributes, ObjectChecksum,
    ObjectTransferMetadata, ObjectWriteEncryption, ObjectWriteOptions, SseCustomerKey,
    TaggingDirective, TransferCopyOptions, TransferReadOptions,
};
pub use undo::{
    UndoAction, UndoObjectResult, UndoOutcome, UndoPlan, UndoPlanItem, plan_object_undo,
};
pub use watch::{WatchApi, WatchEvent, WatchFrame, WatchRequest, WatchSource, WatchStream};

pub mod catalog;
