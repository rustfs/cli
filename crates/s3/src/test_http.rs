//! SDK test connectors owned by rc; request capture needs no protocol-format dependency.

use aws_smithy_runtime_api::client::http::{
    HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpConnector,
};
use aws_smithy_runtime_api::client::orchestrator::{HttpRequest, HttpResponse};
use aws_smithy_runtime_api::client::result::ConnectorError;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_types::body::SdkBody;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub(crate) struct StaticReplayClient {
    responses: Arc<Mutex<VecDeque<HttpResponse>>>,
    requests: Arc<Mutex<Vec<Arc<HttpRequest>>>>,
}

pub(crate) struct ReplayEvent {
    response: HttpResponse,
}

impl ReplayEvent {
    pub(crate) fn new(_request: http::Request<SdkBody>, response: http::Response<SdkBody>) -> Self {
        Self {
            response: response.try_into().expect("valid SDK response"),
        }
    }
}

impl StaticReplayClient {
    pub(crate) fn new(events: Vec<ReplayEvent>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(
                events.into_iter().map(|event| event.response).collect(),
            )),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn actual_requests(&self) -> impl Iterator<Item = Arc<HttpRequest>> + use<> {
        self.requests
            .lock()
            .expect("request capture lock")
            .clone()
            .into_iter()
    }
}

impl HttpConnector for StaticReplayClient {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        self.requests
            .lock()
            .expect("request capture lock")
            .push(Arc::new(request));
        let response = self
            .responses
            .lock()
            .expect("response queue lock")
            .pop_front();
        HttpConnectorFuture::ready(
            response.ok_or_else(|| ConnectorError::other("no test response remains".into(), None)),
        )
    }
}

impl HttpClient for StaticReplayClient {
    fn http_connector(
        &self,
        _settings: &HttpConnectorSettings,
        _components: &RuntimeComponents,
    ) -> SharedHttpConnector {
        SharedHttpConnector::new(self.clone())
    }
}

pub(crate) struct CaptureRequestReceiver {
    client: StaticReplayClient,
}

impl CaptureRequestReceiver {
    pub(crate) fn expect_request(self) -> Arc<HttpRequest> {
        let requests = self.client.requests.lock().expect("request capture lock");
        assert_eq!(requests.len(), 1, "expected exactly one SDK request");
        requests[0].clone()
    }

    pub(crate) fn expect_no_request(self) {
        assert_eq!(
            self.client.actual_requests().count(),
            0,
            "expected no SDK request"
        );
    }
}

pub(crate) fn capture_request(
    response: Option<http::Response<SdkBody>>,
) -> (StaticReplayClient, CaptureRequestReceiver) {
    let response = response.unwrap_or_else(|| http::Response::new(SdkBody::empty()));
    let client = StaticReplayClient::new(vec![ReplayEvent::new(
        http::Request::new(SdkBody::empty()),
        response,
    )]);
    (client.clone(), CaptureRequestReceiver { client })
}
