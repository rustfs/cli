use super::*;
use crate::pool::Pools;
use rustfs_nixl::{NixlConfig, NixlServer, RdmaToken};

#[tokio::test]
async fn rustfs_reads_and_writes_the_advertised_host_window() {
    let _guard = crate::pool::NATIVE_TEST_LOCK.lock().await;
    let client = Pools::default();
    let peer = NixlServer::new(&NixlConfig {
        agent_name: "rc-interop-peer".into(),
        chunk_bytes: 4096,
        pool_bytes: 4096,
        ..Default::default()
    })
    .unwrap();
    let mut source = client.reserve(6).await.unwrap();
    source.writable().unwrap()[..6].copy_from_slice(b"upload");
    let mut fields = HeaderMap::new();
    let exposed = source.expose(&Method::PUT, 6, &mut fields).unwrap();
    let token = RdmaToken::from_headers(
        fields[NIXL_AGENT_HEADER].to_str().unwrap(),
        Some(fields[NIXL_METADATA_HEADER].to_str().unwrap()),
        fields[NIXL_BUFFER_HEADER].to_str().unwrap(),
    )
    .unwrap();

    let staging = peer.try_reserve().unwrap();
    let mut staging = peer
        .read_from_client_owned("input", &token, 0, 6, staging)
        .await
        .unwrap();
    assert_eq!(staging.staged().unwrap(), b"upload");
    staging.clear();
    staging.append(b"result").unwrap();
    let (written, _) = peer
        .write_to_client_owned("output", &token, 0, staging)
        .await
        .unwrap();
    assert_eq!(written, 6);
    let mut source = exposed.finished().unwrap();
    source.finish_read(6).unwrap();
    assert_eq!(source.received().unwrap(), b"result");
}
