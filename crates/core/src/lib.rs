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

pub mod alias;
pub mod config;
pub mod error;
pub mod path;
pub mod traits;

pub use alias::{Alias, AliasManager};
pub use config::{Config, ConfigManager};
pub use error::{Error, Result};
pub use path::{parse_path, RemotePath};
pub use traits::{Capabilities, ListOptions, ListResult, ObjectInfo, ObjectStore};
