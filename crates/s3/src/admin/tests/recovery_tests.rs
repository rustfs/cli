use super::*;
use rc_core::admin::recovery::*;
use serde_json::{Value, json};

const ID: &str = "00000000-0000-4000-8000-000000000001";

fn recovery_client(endpoint: &str) -> AdminClient {
    let mut client = admin_client_for_endpoint(endpoint);
    client.http_client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("direct test HTTP client");
    client
}

fn observation(reply: Value) -> String {
    json!({"contractVersion":1,"topologyFingerprint":"topology","observationStartedAtMs":10,
        "observationFinishedAtMs":12,"expectedNodes":1,"pageComplete":true,"nextCursor":null,
        "nodes":[{"nodeId":"a".repeat(64),"host":"node-a","state":"ok","reply":reply}]})
    .to_string()
}

fn summary() -> Value {
    json!({"result":"summary","summary":{"contractVersion":1,"sampledAtMs":9,"checkpointReadable":true,
        "checkpointOwner":ID,"checkpointSequence":1,"activeResponsibilityCount":0,"heldCount":2,
        "legacyGenerationUnknownCount":1,"bucketIncarnationChangedCount":0,"operatorAcceptedCount":1,
        "bestEffortQueueDepth":0,"responsibilityBytes":200}})
}

fn request() -> IntegrityJobRequest {
    IntegrityJobRequest {
        mode: "audit".into(),
        items: vec![IntegrityItem {
            key: "key + & 文".into(),
            version_id: Some("null".into()),
            expected_sha256: Some("00".repeat(32)),
            target_key: None,
        }],
        bytes_per_second: 65536,
        max_object_bytes: 1000,
    }
}

fn job() -> String {
    let mut request = request();
    request.validate_and_normalize().expect("normalize request");
    json!({"format_version":1,"id":ID,"bucket":"bucket","bucket_incarnation":ID,"request":request,
        "state":"paused","revision":0,"results":[{"state":"pending","source_fingerprint":null,
        "expected_sha256":null,"evidence_source":null,"source_size":null,"target_version_id":null,"detail":null}],"last_error":null}).to_string()
}

#[tokio::test]
async fn recovery_summary_uses_v4_with_signed_encoded_cursor() {
    let (endpoint, received, handle) =
        start_admin_owned_test_server("200 OK", "application/json", observation(summary()));
    let result = recovery_client(&endpoint)
        .mrf_summary(Some("a+=&"))
        .await
        .expect("summary");
    assert!(
        matches!(&result.nodes[0].reply, Some(MrfReply::Summary {summary}) if summary.held_count == 2)
    );
    let request = received.recv().expect("captured request");
    assert_eq!(
        request.target,
        "/rustfs/admin/v4/heal/mrf/cluster?cursor=a%2B%3D%26"
    );
    assert!(
        request
            .headers
            .to_lowercase()
            .contains("authorization: aws4-hmac-sha256")
    );
    handle.join().expect("server stopped");
}

#[tokio::test]
async fn recovery_get_and_action_are_bound_to_exact_node_and_id() {
    let node = "a".repeat(64);
    let (endpoint, received, handle) = start_admin_owned_test_server(
        "200 OK",
        "application/json",
        observation(
            json!({"result":"get","observation":{"contractVersion":1,"observedAtMs":10,"responsibility":null}}),
        ),
    );
    let result = recovery_client(&endpoint)
        .mrf_get(&node, ID)
        .await
        .expect("not observed is a valid observation");
    assert!(
        matches!(&result.nodes[0].reply, Some(MrfReply::Get {observation}) if observation.responsibility.is_none())
    );
    assert_eq!(
        received.recv().expect("request").target,
        format!("/rustfs/admin/v4/heal/mrf/cluster/responsibilities/{node}/{ID}")
    );
    handle.join().expect("server stopped");

    let (endpoint, received, handle) = start_admin_owned_test_server(
        "200 OK",
        "application/json",
        observation(
            json!({"result":"action","action":{"responsibilityId":ID,"state":"refreshed","dataVerified":false,"durableResponsibilityRetained":true}}),
        ),
    );
    let action = MrfAction {
        node_id: node,
        request: MrfActionRequest {
            action: "refresh".into(),
            responsibility_id: ID.into(),
            expected_bucket_incarnation_id: None,
            acknowledge_unverified_data_risk: None,
            acknowledge_unknown_source_incarnation: None,
            acknowledge_bucket_incarnation_mismatch: None,
            reason: None,
            reference: None,
            request_id: None,
        },
    };
    recovery_client(&endpoint)
        .mrf_action(&action)
        .await
        .expect("action receipt");
    let request = received.recv().expect("request");
    assert_eq!(request.method, "POST");
    assert_eq!(
        serde_json::from_slice::<Value>(&request.body).expect("body"),
        serde_json::to_value(action).expect("action")
    );
    handle.join().expect("server stopped");
}

#[tokio::test]
async fn recovery_rejects_response_variant_or_job_identity_substitution() {
    let (endpoint, _received, handle) = start_admin_owned_test_server(
        "200 OK",
        "application/json",
        observation(
            json!({"result":"action","action":{"responsibilityId":ID,"state":"refreshed","dataVerified":false,"durableResponsibilityRetained":true}}),
        ),
    );
    assert!(recovery_client(&endpoint).mrf_summary(None).await.is_err());
    handle.join().expect("server stopped");
    let (endpoint, _received, handle) =
        start_admin_owned_test_server("200 OK", "application/json", job());
    assert!(
        recovery_client(&endpoint)
            .integrity_job("different-bucket", ID)
            .await
            .is_err()
    );
    handle.join().expect("server stopped");
}

#[tokio::test]
async fn recovery_create_normalizes_digest_preserves_null_slot_and_uses_v3() {
    let (endpoint, received, handle) =
        start_admin_owned_test_server("201 Created", "application/json", job());
    let result = recovery_client(&endpoint)
        .integrity_create("bucket", &request())
        .await
        .expect("paused job");
    assert_eq!(result.state, "paused");
    let request = received.recv().expect("request");
    assert_eq!(request.target, "/rustfs/admin/v3/integrity/bucket/jobs");
    let body: Value = serde_json::from_slice(&request.body).expect("body");
    assert_eq!(body["items"][0]["version_id"], "null");
    assert_eq!(
        body["items"][0]["expected_sha256"],
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
    );
    assert!(
        request
            .headers
            .to_lowercase()
            .contains("authorization: aws4-hmac-sha256")
    );
    handle.join().expect("server stopped");
}

#[tokio::test]
async fn recovery_inventory_obeys_server_limit_and_encodes_markers() {
    let body = json!({"observed_at":"now","bucket_incarnation":ID,"items":[],"is_truncated":false,"next_key_marker":null,"next_version_marker":null}).to_string();
    let (endpoint, received, handle) =
        start_admin_owned_test_server("200 OK", "application/json", body);
    let mut query = IntegrityInventoryQuery {
        prefix: "a&".into(),
        key_marker: Some("k+".into()),
        version_marker: Some("null".into()),
        limit: 100,
    };
    recovery_client(&endpoint)
        .integrity_inventory("bucket", &query)
        .await
        .expect("limit 100");
    let target = received.recv().expect("request").target;
    assert!(
        target.contains("prefix=a%26")
            && target.contains("key-marker=k%2B")
            && target.contains("limit=100")
    );
    handle.join().expect("server stopped");
    query.limit = 101;
    let error = recovery_client("http://127.0.0.1:1")
        .integrity_inventory("bucket", &query)
        .await
        .expect_err("local limit rejection");
    assert_eq!(error.exit_code(), 2);
}

#[tokio::test]
async fn recovery_transport_bounds_responses_and_preserves_safe_error_codes() {
    for (status, body, code) in [
        (
            "403 Forbidden",
            r#"{"Code":"AccessDenied","Message":"DO-NOT-ECHO"}"#,
            4,
        ),
        ("404 Not Found", r#"{"Code":"NoSuchKey"}"#, 5),
        ("404 Not Found", "unsupported endpoint", 7),
        ("412 Precondition Failed", "DO-NOT-ECHO", 6),
    ] {
        let (endpoint, _received, handle) = start_admin_test_server(status, body);
        let error = recovery_client(&endpoint)
            .integrity_job("bucket", ID)
            .await
            .expect_err("rejected request");
        assert_eq!(error.exit_code(), code);
        assert!(!error.to_string().contains("DO-NOT-ECHO"));
        handle.join().expect("server stopped");
    }
    let (endpoint, _received, handle) =
        start_admin_declared_length_server("200 OK", 8 * 1024 * 1024 + 1);
    assert!(
        recovery_client(&endpoint)
            .integrity_readiness()
            .await
            .is_err()
    );
    handle.join().expect("server stopped");
}

#[tokio::test]
async fn recovery_decode_errors_do_not_echo_untrusted_field_names() {
    let mut response: Value = serde_json::from_str(&job()).expect("job fixture");
    response["request"]["DO-NOT-ECHO"] = json!("untrusted response");
    let (endpoint, _received, handle) =
        start_admin_owned_test_server("200 OK", "application/json", response.to_string());
    let error = recovery_client(&endpoint)
        .integrity_job("bucket", ID)
        .await
        .expect_err("unknown request field");
    assert!(!error.to_string().contains("DO-NOT-ECHO"));
    handle.join().expect("server stopped");
}

#[tokio::test]
async fn recovery_total_deadline_covers_stalled_and_dripping_bodies_without_retry() {
    for drip in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = format!("http://{}", listener.local_addr().expect("address"));
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("connection");
            sender
                .send(read_admin_request(&mut stream))
                .expect("captured request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n")
                .expect("headers");
            for _ in 0..8 {
                thread::sleep(Duration::from_millis(40));
                if drip && stream.write_all(b" ").is_err() {
                    break;
                }
            }
            listener.set_nonblocking(true).expect("nonblocking");
            assert!(listener.accept().is_err(), "mutation must not retry");
        });
        let error = recovery_client(&endpoint)
            .recovery_json_with_budget::<Value>(
                Method::POST,
                (false, "/integrity/bucket/jobs"),
                &[],
                Some(b"{}".to_vec()),
                Duration::from_millis(100),
            )
            .await
            .expect_err("total deadline");
        assert!(matches!(error, Error::Network(_)));
        assert!(error.to_string().contains("total time budget"));
        assert_eq!(receiver.recv().expect("request").method, "POST");
        handle.join().expect("server stopped");
    }
}
