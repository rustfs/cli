//! rc-s3: S3 SDK adapter for rc CLI client
//!
//! This crate provides the implementation of the ObjectStore trait
//! using the aws-sdk-s3 crate. It is the only crate that directly
//! depends on the AWS SDK.

pub mod admin;
pub mod client;
pub mod data_transport;
mod lifecycle_xml;
pub mod multipart;
mod ops;
mod payload_checksum;
mod select;
mod watch;

pub use admin::AdminClient;
pub use client::{DeleteRequestOptions, S3Client};
pub use multipart::{MultipartConfig, UploadState};
