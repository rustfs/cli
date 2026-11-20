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
    let transport = Factory.create(endpoint.clone()).unwrap();
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
