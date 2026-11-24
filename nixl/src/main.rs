//! rc with native S3-over-RDMA providers.
#![forbid(unsafe_code)]

mod buffer;
#[cfg(test)]
mod loopback;
#[cfg(feature = "cuobject")]
mod nvidia;
mod pool;
mod transport;
mod wire;

use rc_core::RdmaBackend;

fn register(backend: RdmaBackend) {
    let factory = std::sync::Arc::new(transport::Factory::new(backend));
    let installed = match backend {
        RdmaBackend::Nixl => rc_s3::data_transport::install_trusted_nixl_factory(factory),
        RdmaBackend::CuObject => rc_s3::data_transport::install_rdma_backend(backend, factory),
    };
    if let Err(error) = installed {
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
