//! rc with native S3-over-RDMA providers.
#![forbid(unsafe_code)]

use rustfs_rdma_client::{RdmaBackend, RdmaProvider};
use std::sync::Arc;

fn register(backend: RdmaBackend) {
    let result = RdmaProvider::new(backend).and_then(|provider| {
        rc_s3::data_transport::install_rdma_backend(backend, Arc::new(provider))
    });
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(error.exit_code());
    }
}

fn main() {
    #[cfg(feature = "nixl")]
    register(RdmaBackend::Nixl);
    #[cfg(feature = "cuobject")]
    register(RdmaBackend::CuObject);
    rustfs_cli::run();
}
