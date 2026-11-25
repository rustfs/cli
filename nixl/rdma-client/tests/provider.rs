//! Exercise the SDK through its public API in a separate test executable.
use rc_s3::data_transport::{
    ControlExecutor, ObjectDataTransport, ObjectDataTransportFactory, install_rdma_backend,
};
use rustfs_rdma_client::{
    Alias, DataTransportMode, Error, ObjectStore, RdmaBackend, RdmaProvider, RemotePath, Result,
};
use std::sync::Arc;

#[test]
fn selecting_a_backend_requires_its_feature() {
    for (backend, enabled) in [
        (RdmaBackend::Nixl, cfg!(feature = "nixl")),
        (RdmaBackend::CuObject, cfg!(feature = "cuobject")),
    ] {
        match RdmaProvider::new(backend) {
            Ok(_) => assert!(enabled),
            Err(Error::UnsupportedFeature(_)) => assert!(!enabled),
            Err(error) => panic!("unexpected provider error: {error}"),
        }
    }
}

#[tokio::test]
async fn sdk_clients_use_the_explicit_provider_even_with_a_global_factory() {
    struct FailingFactory;
    impl ObjectDataTransportFactory for FailingFactory {
        fn create(&self, _: Arc<dyn ControlExecutor>) -> Result<Arc<dyn ObjectDataTransport>> {
            Err(Error::General("global factory selected".into()))
        }
    }
    install_rdma_backend(RdmaBackend::Nixl, Arc::new(FailingFactory)).unwrap();
    rc_core::set_global_data_transport_mode(DataTransportMode::Rdma);
    let mut alias = Alias::new("test", "https://127.0.0.1:1", "test", "test");
    assert!(
        matches!(rc_s3::S3Client::new(alias.clone()).await, Err(Error::General(message)) if message == "global factory selected")
    );
    let backend = if cfg!(feature = "nixl") {
        RdmaBackend::Nixl
    } else {
        RdmaBackend::CuObject
    };
    alias.anonymous = true;
    let client = RdmaProvider::new(backend)
        .unwrap()
        .client(alias, DataTransportMode::Rdma)
        .await
        .unwrap();
    let path = RemotePath::new("test", "bucket", "object");
    let error = client.get_object(&path).await.unwrap_err();
    assert!(
        matches!(error, Error::DataTransfer { source: Some(source), .. }
        if matches!(*source, Error::RequestRejected(_)))
    );
}
