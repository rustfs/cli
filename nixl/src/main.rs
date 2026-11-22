//! rc with native S3-over-RDMA providers.
#![forbid(unsafe_code)]

mod buffer;
#[cfg(feature = "cuobject")]
mod nvidia;
mod pool;
mod transport;
mod wire;

use rc_core::RdmaBackend;

fn register(backend: RdmaBackend) {
    let factory = std::sync::Arc::new(transport::Factory::new(backend));
    if let Err(error) = rc_s3::data_transport::install_rdma_backend(backend, factory) {
        eprintln!("{error}");
        std::process::exit(error.exit_code());
    }
}

fn main() {
    register(RdmaBackend::Nixl);
    #[cfg(feature = "cuobject")]
    register(RdmaBackend::CuObject);
    rustfs_cli::run();
}
