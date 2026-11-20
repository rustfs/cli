//! rc with the RustFS native S3-over-NIXL/UCX transport.
#![forbid(unsafe_code)]

mod pool;
mod transport;
mod wire;

fn main() {
    if let Err(error) = rc_s3::data_transport::install_object_data_transport_factory(
        std::sync::Arc::new(transport::Factory),
    ) {
        eprintln!("{error}");
        std::process::exit(error.exit_code());
    }
    rustfs_cli::run();
}
