//! Wire-level tests against a local HTTP server: signing, routes, query
//! encoding, the plaintext body, and the fixture-pinned responses.

use super::*;
use rc_core::Alias;
use rc_core::admin::{
    PathStyle, SkipExisting, SourceCredentialsRequest, SourceProvider, SourceRequest, TlsRequest,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SET_REQUEST: &str =
    include_str!("../../../../core/tests/fixtures/on_demand_migration/set_request.json");
const SET_RESPONSE: &str =
    include_str!("../../../../core/tests/fixtures/on_demand_migration/set_response.json");
const GET_RESPONSE: &str =
    include_str!("../../../../core/tests/fixtures/on_demand_migration/get_response.json");
const STATUS: &str =
    include_str!("../../../../core/tests/fixtures/on_demand_migration/status.json");
const BACKFILL_JOB: &str =
    include_str!("../../../../core/tests/fixtures/on_demand_migration/backfill_job.json");

struct Captured {
    method: String,
    target: String,
    headers: String,
    body: Vec<u8>,
}

async fn server(
    responses: Vec<(u16, String)>,
) -> (AdminClient, tokio::task::JoinHandle<Vec<Captured>>) {
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
            let header_end = loop {
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
                        break end + 4;
                    }
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
            let mut request_line = headers.lines().next().unwrap().split_whitespace();
            requests.push(Captured {
                method: request_line.next().unwrap().to_string(),
                target: request_line.next().unwrap().to_string(),
                headers: headers.clone(),
                body: bytes[header_end..].to_vec(),
            });
            let payload = if status == 204 { String::new() } else { body };
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
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

fn fixture_request() -> OnDemandMigrationConfigRequest {
    let mut request = OnDemandMigrationConfigRequest::new(SourceRequest {
        provider: SourceProvider::Minio,
        endpoint: Some("https://source.example.com:9000".into()),
        region: "us-east-1".into(),
        bucket: "legacy-photos".into(),
        path_style: PathStyle::Auto,
        credentials: Some(SourceCredentialsRequest {
            access_key: "AKIASOURCE".into(),
            secret_key: Zeroizing::new("sourceSecretKey123".into()),
            session_token: None,
        }),
        tls: TlsRequest::default(),
    });
    request.filter.source_prefix = Some("photos/".into());
    request
}

#[tokio::test]
async fn set_signs_a_put_with_the_fixture_body_and_dry_run_query() {
    let (client, server) = server(vec![(200, SET_RESPONSE.to_string())]).await;
    let result = client
        .set_on_demand_migration("photos", &fixture_request(), true)
        .await
        .unwrap();
    assert_eq!(result.bucket, "photos");
    assert!(result.probe.unwrap().reachable);
    let requests = server.await.unwrap();
    let request = &requests[0];
    assert_eq!(request.method, "PUT");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/on-demand-migration/photos?dry-run=true"
    );
    let lower = request.headers.to_ascii_lowercase();
    assert!(lower.contains("authorization: aws4-hmac-sha256"));
    assert!(lower.contains("content-type: application/json"));
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    let expected: Value = serde_json::from_str(SET_REQUEST).unwrap();
    assert_eq!(body, expected);
}

#[tokio::test]
async fn set_without_dry_run_has_no_query_and_encodes_the_bucket() {
    let (client, server) = server(vec![(200, SET_RESPONSE.to_string())]).await;
    client
        .set_on_demand_migration("my.bucket", &fixture_request(), false)
        .await
        .unwrap();
    let requests = server.await.unwrap();
    assert_eq!(
        requests[0].target,
        "/rustfs/admin/v3/on-demand-migration/my.bucket"
    );
}

#[tokio::test]
async fn get_and_status_use_their_routes_and_parse_fixtures() {
    let (client, server) = server(vec![
        (200, GET_RESPONSE.to_string()),
        (200, STATUS.to_string()),
    ])
    .await;
    let config = client.get_on_demand_migration("photos").await.unwrap();
    assert_eq!(config.updated_at.as_deref(), Some("2026-09-02T10:00:00Z"));
    let status = client.on_demand_migration_status("photos").await.unwrap();
    assert_eq!(status.served_by_source_ratio, None);
    assert_eq!(status.queue_depth, 1);
    let requests = server.await.unwrap();
    assert_eq!(requests[0].method, "GET");
    assert_eq!(
        requests[0].target,
        "/rustfs/admin/v3/on-demand-migration/photos"
    );
    assert_eq!(
        requests[1].target,
        "/rustfs/admin/v3/on-demand-migration/photos/status"
    );
}

#[tokio::test]
async fn delete_accepts_no_content() {
    let (client, server) = server(vec![(204, String::new())]).await;
    client.delete_on_demand_migration("photos").await.unwrap();
    let requests = server.await.unwrap();
    assert_eq!(requests[0].method, "DELETE");
    assert!(requests[0].body.is_empty());
}

#[tokio::test]
async fn backfill_routes_carry_the_op_selector_and_optional_body() {
    let (client, server) = server(vec![
        (200, BACKFILL_JOB.to_string()),
        (200, BACKFILL_JOB.to_string()),
        (200, BACKFILL_JOB.to_string()),
    ])
    .await;
    let request = BackfillStartRequest {
        prefix: Some("photos/".into()),
        skip_existing: Some(SkipExisting::EtagOrSize),
        dry_run: true,
    };
    let started = client
        .start_on_demand_migration_backfill("photos", &request)
        .await
        .unwrap();
    assert_eq!(started.job.unwrap().state, "running");
    client
        .cancel_on_demand_migration_backfill("photos")
        .await
        .unwrap();
    client
        .on_demand_migration_backfill_status("photos")
        .await
        .unwrap();
    let requests = server.await.unwrap();
    assert_eq!(requests[0].method, "POST");
    assert_eq!(
        requests[0].target,
        "/rustfs/admin/v3/on-demand-migration/photos/backfill?op=start"
    );
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"prefix":"photos/","skip_existing":"etag_or_size","dry_run":true})
    );
    assert_eq!(requests[1].method, "POST");
    assert_eq!(
        requests[1].target,
        "/rustfs/admin/v3/on-demand-migration/photos/backfill?op=cancel"
    );
    assert!(requests[1].body.is_empty());
    assert_eq!(requests[2].method, "GET");
    assert_eq!(
        requests[2].target,
        "/rustfs/admin/v3/on-demand-migration/photos/backfill"
    );
}

#[tokio::test]
async fn route_absence_is_reported_as_unsupported_not_as_not_found() {
    let (client, server) = server(vec![(404, String::new())]).await;
    let error = client.get_on_demand_migration("photos").await.unwrap_err();
    assert_eq!(error.exit_code(), 7);
    assert!(error.to_string().contains(UNSUPPORTED_MESSAGE));
    server.await.unwrap();
}

#[tokio::test]
async fn unset_configuration_is_not_found() {
    let (client, server) = server(vec![(
        404,
        r#"{"Code":"NoSuchConfiguration","Message":"on-demand migration is not configured for bucket photos"}"#.into(),
    )])
    .await;
    let error = client.get_on_demand_migration("photos").await.unwrap_err();
    assert_eq!(error.exit_code(), 5);
    assert!(error.to_string().contains("NoSuchConfiguration"));
    server.await.unwrap();
}

#[tokio::test]
async fn backfill_conflict_and_source_unreachable_keep_their_classes() {
    let (client, server) = server(vec![
        (
            409,
            r#"{"Code":"OnDemandMigrationBackfillRunning","Message":"a backfill job is already running"}"#.into(),
        ),
        (
            400,
            r#"{"Code":"OnDemandMigrationSourceUnreachable","Message":"connect"}"#.into(),
        ),
        (403, r#"{"Code":"AccessDenied","Message":"licence"}"#.into()),
    ])
    .await;
    let conflict = client
        .start_on_demand_migration_backfill("photos", &BackfillStartRequest::default())
        .await
        .unwrap_err();
    assert_eq!(conflict.exit_code(), 6);
    let unreachable = client
        .set_on_demand_migration("photos", &fixture_request(), true)
        .await
        .unwrap_err();
    assert_eq!(unreachable.exit_code(), 3);
    let licence = client
        .set_on_demand_migration("photos", &fixture_request(), false)
        .await
        .unwrap_err();
    assert_eq!(licence.exit_code(), 4);
    server.await.unwrap();
}

#[tokio::test]
async fn error_bodies_never_echo_the_admin_credentials() {
    let (client, server) = server(vec![(
        400,
        r#"{"Code":"InvalidArgument","Message":"signed with test-secret by test-access"}"#.into(),
    )])
    .await;
    let error = client.get_on_demand_migration("photos").await.unwrap_err();
    let text = error.to_string();
    assert!(!text.contains("test-secret"));
    assert!(!text.contains("test-access"));
    assert!(text.contains("[REDACTED]"));
    server.await.unwrap();
}

#[tokio::test]
async fn invalid_bucket_or_config_never_reaches_the_network() {
    let (client, server) = server(vec![]).await;
    assert_eq!(
        client
            .get_on_demand_migration("a/b")
            .await
            .unwrap_err()
            .exit_code(),
        2
    );
    let mut request = fixture_request();
    request.source.endpoint = None;
    assert_eq!(
        client
            .set_on_demand_migration("photos", &request, false)
            .await
            .unwrap_err()
            .exit_code(),
        2
    );
    assert!(server.await.unwrap().is_empty());
}
