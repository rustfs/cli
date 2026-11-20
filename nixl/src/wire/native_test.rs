use super::*;
use crate::pool::Pool;
use rustfs_nixl::{NixlConfig, NixlServer, RdmaToken};

#[tokio::test]
async fn rustfs_reads_and_writes_the_advertised_host_window() {
    let client = Pool::open(4096).unwrap();
    let peer = NixlServer::new(&NixlConfig {
        agent_name: "rc-interop-peer".into(),
        chunk_bytes: 4096,
        pool_bytes: 4096,
        ..Default::default()
    })
    .unwrap();
    let mut source = client.reserve().await.unwrap();
    source.append(b"upload").unwrap();
    let mut fields = HeaderMap::new();
    bind(
        &mut fields,
        &client.name,
        &client.metadata,
        source.addr(),
        6,
    )
    .unwrap();
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
    assert_eq!(source.staged().unwrap(), b"result");
}
