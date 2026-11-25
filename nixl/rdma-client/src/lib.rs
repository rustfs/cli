//! Linux S3 object transfers using NIXL/UCX or NVIDIA cuObject host memory.
//!
//! Select a backend when creating a provider and a transfer policy per client.
//! The S3 client handles credentials, TLS, signing and multipart publication.
//!
//! ```no_run
//! use rustfs_rdma_client::{Alias, DataTransportMode, ObjectStore, RdmaBackend, RdmaProvider, RemotePath, Result};
//!
//! async fn download(alias: Alias, backend: RdmaBackend, path: &RemotePath) -> Result<Vec<u8>> {
//!     let provider = RdmaProvider::new(backend)?;
//!     let client = provider.client(alias, DataTransportMode::Rdma).await?;
//!     client.get_object(path).await
//! }
//! ```
#![forbid(unsafe_code)]

#[cfg(not(any(feature = "nixl", feature = "cuobject")))]
compile_error!("Enable the nixl or cuobject feature to select a native transport");

mod buffer;
#[cfg(feature = "cuobject")]
mod nvidia;
mod pool;
mod transport;
mod wire;

pub use rc_core::{Alias, DataTransportMode, Error, ObjectStore, RdmaBackend, RemotePath, Result};
pub use rc_s3::S3Client;
pub use transport::RdmaProvider;
