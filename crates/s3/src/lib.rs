//! rc-s3: S3 SDK adapter for rc CLI client
//!
//! This crate provides the implementation of the ObjectStore trait
//! using the aws-sdk-s3 crate. It is the only crate that directly
//! depends on the AWS SDK.

pub mod capability;
pub mod client;

pub use client::S3Client;
