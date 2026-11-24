//! Exercise registered transfers through a real HTTP client and a local test peer.

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderValue, StatusCode};
use http_body_util::Full;
use hyper::{Request, Response, body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use rc_core::{Error, Result};
use rc_s3::data_transport::{
    ControlExecutor, ControlRequest, ControlResponse, NixlSigningControl, NixlSigningRequest,
    ObjectDataTransport, RegisteredControlResponse, TrustedNixlFactory,
};
use std::sync::Arc;
use tokio::net::TcpListener;

struct Loopback(Arc<dyn ControlExecutor>);

impl crate::transport::Factory {
    pub(crate) fn with_test_peer(
        &self,
        endpoint: Arc<dyn ControlExecutor>,
    ) -> Result<Arc<dyn ObjectDataTransport>> {
        let adapter = Arc::new(Loopback(endpoint));
        self.create_with_signer(adapter.clone(), adapter)
    }
}

#[async_trait]
impl ControlExecutor for Loopback {
    fn endpoint(&self) -> Option<&str> {
        self.0.endpoint()
    }

    async fn execute_headers(&self, request: ControlRequest) -> Result<ControlResponse> {
        self.0.execute_headers(request).await
    }
}

#[async_trait]
impl NixlSigningControl for Loopback {
    async fn http_builder(&self) -> Result<reqwest::ClientBuilder> {
        Ok(reqwest::Client::builder().no_proxy())
    }

    async fn prepare(&self, request: ControlRequest) -> Result<NixlSigningRequest> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let mut url = reqwest::Url::parse(&format!("http://{}/", listener.local_addr()?))?;
        url.set_path(&format!("/{}/{}", request.path.bucket, request.path.key));
        url.query_pairs_mut().extend_pairs(&request.query);
        let endpoint = Arc::clone(&self.0);
        let path = request.path;
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept local request");
            let service = service_fn(move |incoming: Request<Incoming>| {
                let endpoint = Arc::clone(&endpoint);
                let path = path.clone();
                async move {
                    let uri = reqwest::Url::parse(&format!("http://localhost{}", incoming.uri()))?;
                    let query = uri.query_pairs().into_owned().collect();
                    let (parts, _) = incoming.into_parts();
                    let signature = parts.headers["authorization"].to_str().unwrap();
                    for name in [
                        rustfs_nixl::NIXL_AGENT_HEADER,
                        rustfs_nixl::NIXL_METADATA_HEADER,
                        rustfs_nixl::NIXL_BUFFER_HEADER,
                    ] {
                        assert!(signature.contains(name));
                    }
                    let request = ControlRequest {
                        path,
                        query,
                        method: parts.method,
                        headers: parts.headers,
                    };
                    let (status, headers, body) = match endpoint.execute_registered(request).await?
                    {
                        RegisteredControlResponse::Response(response) => {
                            (response.status, response.headers, Bytes::new())
                        }
                        RegisteredControlResponse::RejectedBeforeDma(_) => {
                            let mut headers = http::HeaderMap::new();
                            headers.insert(
                                "content-type",
                                HeaderValue::from_static("application/xml"),
                            );
                            (
                                StatusCode::PRECONDITION_FAILED,
                                headers,
                                Bytes::from_static(
                                    b"<Error><Code>PreconditionFailed</Code></Error>",
                                ),
                            )
                        }
                    };
                    let mut response = Response::new(Full::new(body));
                    *response.status_mut() = status;
                    *response.headers_mut() = headers;
                    Ok::<_, Error>(response)
                }
            });
            // Test peers can intentionally close the exchange without a response.
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
        let builder = reqwest::Client::new()
            .request(request.method, url)
            .headers(request.headers);
        Ok(NixlSigningRequest {
            request: builder,
            credentials: aws_credential_types::Credentials::new(
                "test-id",
                "test-secret",
                None,
                None,
                "unit-test",
            ),
            region: "us-east-1".into(),
        })
    }
}

pub(crate) struct Receipt(pub usize);

#[async_trait]
impl ControlExecutor for Receipt {
    async fn execute_headers(&self, _: ControlRequest) -> Result<ControlResponse> {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            rustfs_nixl::RDMA_REPLY_HEADER,
            HeaderValue::from_static("200"),
        );
        headers.insert(
            rustfs_nixl::RDMA_BYTES_TRANSFERRED_HEADER,
            HeaderValue::from(self.0),
        );
        Ok(ControlResponse {
            status: StatusCode::OK,
            headers,
        })
    }
}

pub(crate) async fn transfer(
    peer_service: Arc<dyn ControlExecutor>,
    method: http::Method,
    exposed: &mut crate::pool::Exposed,
) {
    let request = ControlRequest {
        method,
        path: rc_core::RemotePath::new("test", "bucket", "key"),
        query: vec![],
        headers: http::HeaderMap::new(),
    };
    let control = Loopback(peer_service);
    let prepared = control.prepare(request).await.unwrap();
    let http = rustfs_nixl::ClientWindowHttp::new(control.http_builder().await.unwrap()).unwrap();
    let response = exposed.exchange_nixl(&http, prepared).await.unwrap();
    assert_eq!(response.status, StatusCode::OK);
}
