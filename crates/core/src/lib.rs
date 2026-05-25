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
pub mod error;
pub mod lifecycle;
pub mod path;
pub mod replication;
pub mod retry;
pub mod select;
pub mod traits;

pub use alias::{
    Alias, AliasManager, RequestHeader, global_request_headers, set_global_request_headers,
    validate_alias_endpoint,
};
pub use config::{Config, ConfigManager};
pub use cors::{CorsConfiguration, CorsRule};
pub use error::{Error, Result};
pub use lifecycle::{
    LifecycleConfiguration, LifecycleExpiration, LifecycleRule, LifecycleRuleStatus,
    LifecycleTransition, NoncurrentVersionExpiration, NoncurrentVersionTransition,
};
pub use path::{ParsedPath, RemotePath, parse_object_path, parse_path};
pub use replication::{
    BucketTarget, BucketTargetCredentials, ReplicationConfiguration, ReplicationDestination,
    ReplicationRule, ReplicationRuleStatus,
};
pub use retry::{RetryBuilder, is_retryable_error, retry_with_backoff};
pub use select::{
    SelectCompression, SelectCsvFileHeaderInfo, SelectCsvInputOptions, SelectCsvOutputOptions,
    SelectInputFormat, SelectJsonInputOptions, SelectJsonInputType, SelectJsonOutputOptions,
    SelectOptions, SelectOutputFormat, SelectQuoteFields, SelectScanRangeOptions,
    SelectSseCustomerOptions,
};
pub use traits::{
    BucketNotification, Capabilities, ListOptions, ListResult, NotificationTarget, ObjectInfo,
    ObjectStore, ObjectVersion, ObjectVersionListResult,
};
