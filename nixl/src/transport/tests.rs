use super::*;
use std::sync::Mutex;

struct VersionedObject {
    queries: Mutex<Vec<Vec<(String, String)>>>,
    get_version: &'static str,
}

#[async_trait]
impl ControlExecutor for VersionedObject {
    async fn execute_headers(&self, request: ControlRequest) -> Result<ControlResponse> {
        self.queries.lock().unwrap().push(request.query);
        let mut headers = HeaderMap::new();
        let version = if request.method == Method::HEAD {
            "v1"
        } else {
            assert_eq!(request.headers["if-match"], "\"etag\"");
            headers.insert(RDMA_REPLY_HEADER, HeaderValue::from_static("200"));
            self.get_version
        };
        headers.insert("etag", HeaderValue::from_static("\"etag\""));
        headers.insert("content-length", HeaderValue::from_static("0"));
        headers.insert("x-amz-version-id", HeaderValue::from_static(version));
        Ok(ControlResponse {
            status: StatusCode::OK,
            headers,
        })
    }
}

async fn download(
    version_id: Option<&str>,
    get_version: &'static str,
) -> (DataTransportResult<DownloadResult>, Arc<VersionedObject>) {
    let endpoint = Arc::new(VersionedObject {
        queries: Mutex::default(),
        get_version,
    });
    let transport = Factory(RdmaBackend::Nixl).create(endpoint.clone()).unwrap();
    let path = RemotePath::new("test", "bucket", "key");
    let options = rc_core::TransferReadOptions {
        version_id: version_id.map(str::to_owned),
        ..Default::default()
    };
    let result = transport
        .download(
            DownloadRequest {
                path: &path,
                options: &options,
                max_bytes: None,
                max_receive_bytes: 4096,
                on_progress: &mut |_, _| {},
            },
            &mut tokio::io::sink(),
        )
        .await;
    (result, endpoint)
}

#[tokio::test]
async fn current_and_explicit_version_keep_the_callers_selector() {
    for selector in [None, Some("v1")] {
        let (result, endpoint) = download(selector, "v1").await;
        assert!(result.is_ok());
        let query: Vec<_> = selector
            .map(|v| ("versionId".to_owned(), v.to_owned()))
            .into_iter()
            .collect();
        assert_eq!(
            *endpoint.queries.lock().unwrap(),
            vec![query.clone(), query]
        );
    }
}

#[tokio::test]
async fn replacement_with_the_same_etag_is_detected_before_writing() {
    let (result, _) = download(None, "v2").await;
    assert!(
        matches!(result, Err(error) if matches!(error.source.as_deref(), Some(Error::Conflict(_))))
    );
}

#[tokio::test]
async fn a_delete_marker_keeps_its_version_and_exit_code() {
    struct DeleteMarker;
    #[async_trait]
    impl ControlExecutor for DeleteMarker {
        async fn execute_headers(&self, request: ControlRequest) -> Result<ControlResponse> {
            assert_eq!(request.method, Method::HEAD);
            let mut headers = HeaderMap::new();
            headers.insert("x-amz-delete-marker", HeaderValue::from_static("true"));
            headers.insert("x-amz-version-id", HeaderValue::from_static("v2"));
            Ok(ControlResponse {
                status: StatusCode::METHOD_NOT_ALLOWED,
                headers,
            })
        }
    }

    let transport = Factory(RdmaBackend::Nixl)
        .create(Arc::new(DeleteMarker))
        .unwrap();
    let path = RemotePath::new("test", "bucket", "key");
    let options = rc_core::ObjectReadOptions {
        version_id: Some("v2".into()),
    };
    let alias = rc_core::Alias::new("test", "https://127.0.0.1:1", "test", "test");
    let client = rc_s3::S3Client::new(alias).await.unwrap();
    for mode in [
        rc_core::DataTransportMode::Auto,
        rc_core::DataTransportMode::Rdma,
    ] {
        let client = client
            .clone()
            .with_object_data_transport(mode, transport.clone());
        let error = client
            .write_object_to_with_options(&path, &options, &mut tokio::io::sink(), None)
            .await
            .expect_err("a delete marker cannot be read as an object");
        assert_eq!(error.exit_code(), 5, "{mode:?}");
        assert!(
            matches!(error, Error::DataTransfer { source: Some(source), .. }
            if matches!(*source, Error::DeleteMarker { ref version_id, .. } if version_id == "v2"))
        );
    }
}

#[tokio::test]
async fn only_proven_conditional_rejections_reuse_the_registered_window() {
    struct Peer(Mutex<Vec<HeaderValue>>);
    #[async_trait]
    impl ControlExecutor for Peer {
        async fn execute_headers(&self, req: ControlRequest) -> Result<ControlResponse> {
            assert_eq!(req.method, Method::HEAD);
            let mut headers = HeaderMap::new();
            headers.insert("etag", HeaderValue::from_static("\"etag\""));
            headers.insert("content-length", HeaderValue::from_static("0"));
            Ok(ControlResponse {
                status: StatusCode::OK,
                headers,
            })
        }
        async fn execute_registered(
            &self,
            req: ControlRequest,
        ) -> Result<RegisteredControlResponse> {
            assert_eq!(req.method, Method::GET);
            assert!(req.headers.contains_key("if-match"));
            let mut seen = self.0.lock().unwrap();
            seen.push(req.headers[rustfs_nixl::NIXL_BUFFER_HEADER].clone());
            Ok(match seen.len() {
                1 => RegisteredControlResponse::Response(ControlResponse {
                    status: StatusCode::PRECONDITION_FAILED,
                    headers: HeaderMap::new(),
                }),
                2..=19 => RegisteredControlResponse::RejectedBeforeDma(Error::Conflict(
                    "PreconditionFailed".into(),
                )),
                _ => {
                    let mut headers = HeaderMap::new();
                    headers.insert(RDMA_REPLY_HEADER, HeaderValue::from_static("200"));
                    RegisteredControlResponse::Response(ControlResponse {
                        status: StatusCode::OK,
                        headers,
                    })
                }
            })
        }
    }
    let peer = Arc::new(Peer(Mutex::default()));
    let transport = Factory(RdmaBackend::Nixl).create(peer.clone()).unwrap();
    let path = RemotePath::new("test", "bucket", "key");
    let opts = rc_core::TransferReadOptions::default();
    for i in 1..=20 {
        let r = transport
            .download(
                DownloadRequest {
                    path: &path,
                    options: &opts,
                    max_bytes: None,
                    max_receive_bytes: 64,
                    on_progress: &mut |_, _| {},
                },
                &mut tokio::io::sink(),
            )
            .await;
        assert_eq!(r.is_ok(), i == 20);
    }
    let seen = peer.0.lock().unwrap();
    assert_ne!(seen[0], seen[1]);
    assert!(seen[1..].iter().all(|v| v == seen[1]));
}

#[cfg(feature = "cuobject")]
#[tokio::test]
#[ignore = "requires an NVIDIA RDMA device and an authenticated RustFS object"]
async fn rustfs_declines_a_stale_conditional_get_before_dma() {
    let env = |key| {
        std::env::var(key)
            .expect("configure the test endpoint, credentials, CA and existing bucket")
    };
    let mut alias = rc_core::Alias::new(
        "test",
        env("RC_TEST_ENDPOINT"),
        env("AWS_ACCESS_KEY_ID"),
        env("AWS_SECRET_ACCESS_KEY"),
    );
    alias.ca_bundle = Some(env("RC_TEST_CA"));
    let client = rc_s3::S3Client::new(alias).await.unwrap();
    let path = RemotePath::new("test", env("RC_TEST_BUCKET"), "known");
    let transport = Transport {
        control: Arc::new(client),
        pools: Pools::new(RdmaBackend::CuObject),
    };
    for _ in 0..20 {
        let mut headers = HeaderMap::new();
        headers.insert("if-match", HeaderValue::from_static("\"stale\""));
        let exposed = transport
            .pools
            .reserve(4096)
            .await
            .unwrap()
            .expose(&Method::GET, 4096, &mut headers)
            .unwrap();
        let req = wire::request(Method::GET, &path, vec![], headers);
        match transport.exchange(req, exposed).await {
            Err(error) if matches!(error.source.as_deref(), Some(Error::Conflict(_))) => {}
            _ => panic!("expected an authenticated pre-transfer rejection"),
        }
    }
}

#[tokio::test]
async fn large_get_submits_both_ranges_and_publishes_in_order() {
    struct Ranges {
        second: tokio::sync::Notify,
        wrong_checksum: bool,
    }

    #[async_trait]
    impl ControlExecutor for Ranges {
        async fn execute_headers(&self, request: ControlRequest) -> Result<ControlResponse> {
            let total = WINDOW + 19;
            let mut headers = HeaderMap::new();
            headers.insert("etag", HeaderValue::from_static("\"immutable\""));
            if request.method == Method::HEAD {
                headers.insert("content-length", HeaderValue::from(total));
                if self.wrong_checksum {
                    // SHA-256 of the empty payload, while these two ranges are nonempty.
                    headers.insert(
                        "x-amz-checksum-sha256",
                        HeaderValue::from_static("47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="),
                    );
                }
                return Ok(ControlResponse {
                    status: StatusCode::OK,
                    headers,
                });
            }
            assert_eq!(request.headers["if-match"], "\"immutable\"");
            let range = request.headers["range"]
                .to_str()
                .unwrap()
                .strip_prefix("bytes=")
                .unwrap();
            let (start, end) = range.split_once('-').unwrap();
            let (start, end): (usize, usize) = (start.parse().unwrap(), end.parse().unwrap());
            if start == 0 {
                assert_eq!(end, WINDOW - 1);
                tokio::time::timeout(std::time::Duration::from_secs(5), self.second.notified())
                    .await
                    .expect("the second range must enter before the first completes");
            } else {
                assert_eq!((start, end), (WINDOW, total - 1));
                self.second.notify_one();
            }
            headers.insert(
                "content-range",
                format!("bytes {start}-{end}/{total}").parse().unwrap(),
            );
            headers.insert("content-length", HeaderValue::from_static("0"));
            headers.insert(RDMA_REPLY_HEADER, HeaderValue::from_static("206"));
            headers.insert(
                rustfs_nixl::RDMA_BYTES_TRANSFERRED_HEADER,
                HeaderValue::from(end - start + 1),
            );
            Ok(ControlResponse {
                status: StatusCode::PARTIAL_CONTENT,
                headers,
            })
        }
    }

    for wrong_checksum in [false, true] {
        let transport = Factory(RdmaBackend::Nixl)
            .create(Arc::new(Ranges {
                second: tokio::sync::Notify::new(),
                wrong_checksum,
            }))
            .unwrap();
        let mut progress = Vec::new();
        let total = WINDOW as u64 + 19;
        let result = transport
            .download(
                DownloadRequest {
                    path: &RemotePath::new("local", "bucket", "key"),
                    options: &Default::default(),
                    max_bytes: None,
                    max_receive_bytes: total,
                    on_progress: &mut |written, _| progress.push(written),
                },
                &mut tokio::io::sink(),
            )
            .await;
        if wrong_checksum {
            assert!(
                matches!(result, Err(error) if matches!(error.source.as_deref(), Some(Error::General(message)) if message.contains("checksum does not match")))
            );
        } else {
            assert_eq!(result.unwrap().bytes_transferred, total);
        }
        assert_eq!(progress, [WINDOW as u64, total]);
    }
}
