use super::*;
use crate::pool::Pools;
use async_trait::async_trait;
use rc_s3::data_transport::{ControlExecutor, ControlResponse};
use rustfs_nixl::{
    NIXL_AGENT_HEADER, NIXL_BUFFER_HEADER, NIXL_METADATA_HEADER, NixlConfig, NixlServer, RdmaToken,
};
use std::sync::Arc;

struct NativePeer(Arc<NixlServer>);

#[async_trait]
impl ControlExecutor for NativePeer {
    async fn execute_headers(&self, request: ControlRequest) -> Result<ControlResponse> {
        let fields = &request.headers;
        let token = RdmaToken::from_headers(
            fields[NIXL_AGENT_HEADER].to_str().unwrap(),
            Some(fields[NIXL_METADATA_HEADER].to_str().unwrap()),
            fields[NIXL_BUFFER_HEADER].to_str().unwrap(),
        )
        .unwrap();
        let mut staging = self.0.try_reserve().unwrap();
        if request.method == Method::PUT {
            let staged = self
                .0
                .read_from_client_owned("input", &token, 0, 6, staging)
                .await
                .unwrap();
            assert_eq!(staged.staged().unwrap(), b"upload");
        } else {
            staging.append(b"result").unwrap();
            assert_eq!(
                self.0
                    .write_to_client_owned("output", &token, 0, staging)
                    .await
                    .unwrap()
                    .0,
                6
            );
        }
        crate::loopback::Receipt(6).execute_headers(request).await
    }
}

#[tokio::test]
async fn rustfs_reads_and_writes_the_advertised_host_window() {
    let client = Pools::default();
    let endpoint = Arc::new(NativePeer(
        NixlServer::new(&NixlConfig {
            agent_name: "rc-interop-peer".into(),
            chunk_bytes: 4096,
            pool_bytes: 4096,
            ..Default::default()
        })
        .unwrap(),
    ));
    let mut window = client.reserve(6).await.unwrap();
    window.buffer().write_at(0, b"upload").unwrap();
    for method in [Method::PUT, Method::GET] {
        let mut exposed = window.expose(&method, 6, &mut HeaderMap::new()).unwrap();
        crate::loopback::transfer(endpoint.clone(), method, &mut exposed).await;
        window = exposed.finished().unwrap();
    }
    window.finish_read(6).unwrap();
    assert_eq!(
        window
            .buffer()
            .received(6)
            .unwrap()
            .next_chunk()
            .unwrap()
            .unwrap(),
        b"result"
    );
}
