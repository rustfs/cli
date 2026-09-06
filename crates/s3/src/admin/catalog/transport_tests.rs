use super::*;
use rc_core::{
    Alias,
    catalog::{CatalogTarget, ResourceKind},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn server(
    responses: Vec<(u16, Value)>,
) -> (AdminClient, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let (mut stream, _) =
                tokio::time::timeout(std::time::Duration::from_secs(10), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
                if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|s| s.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            requests.push(String::from_utf8(bytes).unwrap());
            let body = if status == 204 {
                String::new()
            } else {
                body.to_string()
            };
            stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    let mut client =
        AdminClient::new(&Alias::new("a", &endpoint, "test-access", "test-secret")).unwrap();
    client.http_client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    (client, task)
}
fn request(op: Op) -> CatalogRequest {
    CatalogRequest::new(
        op,
        CatalogTarget::parse("a/warehouse/sales.eu/orders", ResourceKind::Table).unwrap(),
    )
}

#[tokio::test]
async fn catalog_transport_signs_pages_and_preserves_opaque_tokens() {
    let (client, server) = server(vec![
        (
            200,
            json!({"identifiers":[{"name":"one"}],"next-page-token":"a+/="}),
        ),
        (
            200,
            json!({"identifiers":[{"name":"two"}],"next-page-token":null}),
        ),
    ])
    .await;
    let result = client.catalog(&request(Op::TableList)).await.unwrap();
    assert_eq!(result["identifiers"].as_array().unwrap().len(), 2);
    assert!(result.get("next-page-token").is_none());
    let requests = server.await.unwrap();
    assert!(
        requests[0]
            .starts_with("GET /iceberg/v1/warehouse/namespaces/sales%1Feu/tables?pageSize=1000 ")
    );
    assert!(requests[1].contains("pageToken=a%2B%2F%3D"));
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("authorization: aws4-hmac-sha256")
    );
    assert!(requests[0].contains("/s3/aws4_request"));
}
#[tokio::test]
async fn catalog_transport_single_page_and_later_failure() {
    let (client, task) = server(vec![(
        200,
        json!({"namespaces":[],"next-page-token":"next"}),
    )])
    .await;
    let mut req = request(Op::NamespaceList);
    req.single_page = true;
    assert_eq!(
        client.catalog(&req).await.unwrap()["next-page-token"],
        "next"
    );
    task.await.unwrap();
    let (client, task) = server(vec![
        (
            200,
            json!({"identifiers":[{"name":"one"}],"next-page-token":"next"}),
        ),
        (403, json!({"error":{"message":"permission denied"}})),
    ])
    .await;
    assert_eq!(
        client
            .catalog(&request(Op::TableList))
            .await
            .unwrap_err()
            .exit_code(),
        4
    );
    task.await.unwrap();
}
#[tokio::test]
async fn catalog_transport_rejects_repeated_and_missing_page_fields() {
    let (client, task) = server(vec![
        (200, json!({"identifiers":[],"next-page-token":"same"})),
        (200, json!({"identifiers":[],"next-page-token":"same"})),
    ])
    .await;
    assert!(
        client
            .catalog(&request(Op::TableList))
            .await
            .unwrap_err()
            .to_string()
            .contains("progress")
    );
    task.await.unwrap();
    let (client, task) = server(vec![(200, json!({"next-page-token":null}))]).await;
    assert!(client.catalog(&request(Op::TableList)).await.is_err());
    task.await.unwrap();
}
#[tokio::test]
async fn catalog_transport_conflict_is_not_retried_and_body_keeps_guards() {
    let (client, task) = server(vec![(409, json!({"error":{"message":"stale version"}}))]).await;
    let mut req = request(Op::Commit);
    req.body = Some(
        json!({"expected-version-token":"v1","expected-metadata-location":"s3://warehouse/m1","commit-id":"attempt-1","updates":[]}),
    );
    assert_eq!(client.catalog(&req).await.unwrap_err().exit_code(), 6);
    let requests = task.await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(Some(body), req.body);
}
#[tokio::test]
async fn catalog_transport_empty_delete_and_credential_redaction() {
    let (client, task) = server(vec![(204, Value::Null)]).await;
    assert_eq!(
        client.catalog(&request(Op::TableRemove)).await.unwrap(),
        json!({})
    );
    assert!(task.await.unwrap()[0].contains("purgeRequested=false"));
    let (client,task)=server(vec![(200,json!({"metadata":{"snapshots":[]},"storage-credentials":[{"access-key-id":"temporary"}],"config":{"s3.secret-access-key":"temporary-secret","s3.session-token":"temporary-token","visible":"test-secret"}}))]).await;
    let data = client
        .catalog(&request(Op::TableShow))
        .await
        .unwrap()
        .to_string();
    assert!(!data.contains("temporary"));
    assert!(!data.contains("test-secret"));
    task.await.unwrap();
}
#[tokio::test]
async fn catalog_transport_preserves_permission_and_backing_errors() {
    for (status, code, message) in [
        (403, 4, "table action denied"),
        (406, 7, "requires object-backed catalog"),
        (404, 5, "table not found"),
    ] {
        let (client, task) = server(vec![(status, json!({"error":{"message":message}}))]).await;
        let e = client.catalog(&request(Op::TableShow)).await.unwrap_err();
        assert_eq!(e.exit_code(), code);
        assert!(e.to_string().contains(message));
        task.await.unwrap();
    }
}

#[tokio::test]
async fn catalog_ref_create_preserves_absence_requirement() {
    let (client, task) = server(vec![(409, json!({"error":{"message":"ref exists"}}))]).await;
    let mut req = request(Op::RefSet);
    req.child = Some("release".into());
    req.body = Some(
        json!({"snapshot-id":7,"expected-snapshot-id":null,"type":"tag","commit-id":"ref-create"}),
    );
    assert_eq!(client.catalog(&req).await.unwrap_err().exit_code(), 6);
    let requests = task.await.unwrap();
    assert!(
        requests[0].starts_with("POST /iceberg/v1/warehouse/namespaces/sales%1Feu/tables/orders")
    );
    let body: Value = serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(
        body["requirements"],
        json!([{"type":"assert-ref-snapshot-id","ref":"release","snapshot-id":null}])
    );
    assert_eq!(body["updates"][0]["ref-name"], "release");
}
