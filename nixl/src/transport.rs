//! S3 object selection and native transport installation.

mod download;
mod upload;

use async_trait::async_trait;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use rc_core::{DataTransferState, Error, ObjectInfo, RemotePath, Result};
use rc_s3::data_transport::*;
use rustfs_nixl::RDMA_REPLY_HEADER;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::pool::{Exposed, Pools, WINDOW, Window};
use crate::wire;
use rc_core::RdmaBackend;

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use tokio::sync::{Semaphore, SemaphorePermit};

pub(crate) struct Factory {
    backend: RdmaBackend,
    endpoints: Mutex<HashMap<String, Arc<EndpointSupport>>>,
}

impl Factory {
    pub(crate) fn new(backend: RdmaBackend) -> Self {
        Self {
            backend,
            endpoints: Mutex::default(),
        }
    }

    fn support(&self, endpoint: Option<&str>) -> Arc<EndpointSupport> {
        match endpoint {
            Some(endpoint) => self
                .endpoints
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(endpoint.to_owned())
                .or_default()
                .clone(),
            None => Arc::default(),
        }
    }
}

#[derive(Default)]
struct EndpointSupport {
    get: Capability,
    upload_part: Capability,
}

struct Capability {
    state: AtomicU8,
    probe: Semaphore,
}

impl Default for Capability {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(Self::UNKNOWN),
            probe: Semaphore::new(1),
        }
    }
}

impl Capability {
    const UNKNOWN: u8 = 0;
    const SUPPORTED: u8 = 1;
    const HTTP_ONLY: u8 = 2;

    // Only the first operation probes. Once a response identifies the
    // transport, ordinary concurrent transfers need no permit.
    async fn enter(&self) -> DataTransportResult<Option<SemaphorePermit<'_>>> {
        let permit = if self.state.load(Ordering::Acquire) == Self::UNKNOWN {
            self.probe.acquire().await.ok()
        } else {
            None
        };
        if self.state.load(Ordering::Acquire) == Self::HTTP_ONLY {
            return Err(unavailable(Error::UnsupportedFeature(
                "Endpoint served this operation over HTTP".into(),
            )));
        }
        Ok(permit)
    }

    fn supported(&self) {
        if self
            .state
            .compare_exchange(
                Self::UNKNOWN,
                Self::SUPPORTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.probe.close();
        }
    }

    fn http_only(&self) {
        self.state.store(Self::HTTP_ONLY, Ordering::Release);
        self.probe.close();
    }
}

impl Factory {
    fn create_transport(
        &self,
        control: Arc<dyn ControlExecutor>,
        signer: Option<Arc<dyn NixlSigningControl>>,
    ) -> Arc<dyn ObjectDataTransport> {
        Arc::new(Transport {
            support: self.support(control.endpoint()),
            control,
            signed_requests: signer.map(|signer| SignedRequests {
                signer,
                http: Default::default(),
            }),
            pools: Pools::new(self.backend),
        })
    }
}

impl ObjectDataTransportFactory for Factory {
    fn create(&self, control: Arc<dyn ControlExecutor>) -> Result<Arc<dyn ObjectDataTransport>> {
        Ok(self.create_transport(control, None))
    }
}

impl TrustedNixlFactory for Factory {
    fn create_with_signer(
        &self,
        control: Arc<dyn ControlExecutor>,
        signer: Arc<dyn NixlSigningControl>,
    ) -> Result<Arc<dyn ObjectDataTransport>> {
        Ok(self.create_transport(control, Some(signer)))
    }
}

struct SignedRequests {
    signer: Arc<dyn NixlSigningControl>,
    http: tokio::sync::OnceCell<rustfs_nixl::ClientWindowHttp>,
}

struct Transport {
    signed_requests: Option<SignedRequests>,
    control: Arc<dyn ControlExecutor>,
    pools: Pools,
    support: Arc<EndpointSupport>,
}

fn failed(state: DataTransferState, source: Error) -> DataTransportError {
    DataTransportError {
        state,
        message: "RDMA object transfer".into(),
        source: Some(Box::new(source)),
    }
}

fn unavailable(source: Error) -> DataTransportError {
    failed(DataTransferState::NotAccepted, source)
}
fn rejected(source: Error) -> DataTransportError {
    failed(DataTransferState::ConfirmedFailure, source)
}
fn unknown(source: Error) -> DataTransportError {
    failed(DataTransferState::OutcomeUnknown, source)
}

impl Transport {
    async fn exchange(
        &self,
        mut request: ControlRequest,
        window: Window,
        len: usize,
    ) -> DataTransportResult<(ControlResponse, Exposed)> {
        if window.is_nixl() {
            let method = request.method.clone();
            let signed = self.signed_requests.as_ref().ok_or_else(|| {
                unavailable(Error::UnsupportedFeature(
                    "This executor cannot prepare native control requests".into(),
                ))
            })?;
            let prepared = signed.signer.prepare(request).await.map_err(unavailable)?;
            let http = signed
                .http
                .get_or_try_init(|| async {
                    let builder = signed.signer.http_builder().await?;
                    rustfs_nixl::ClientWindowHttp::new(builder).map_err(crate::buffer::nixl_error)
                })
                .await
                .map_err(unavailable)?;
            let mut exposed = window
                .expose(&method, len, &mut HeaderMap::new())
                .map_err(unavailable)?;
            let response = exposed
                .exchange_nixl(http, prepared)
                .await
                .map_err(unknown)?;
            if response.rejected_before_dma {
                return Err(rejected(Error::Conflict(
                    "The S3 endpoint rejected the conditional GET before transferring data".into(),
                )));
            }
            return Ok((
                ControlResponse {
                    status: response.status,
                    headers: response.headers,
                },
                exposed,
            ));
        }
        let exposed = window
            .expose(&request.method, len, &mut request.headers)
            .map_err(unavailable)?;
        match self.control.execute_registered(request).await {
            Ok(RegisteredControlResponse::Response(response)) => Ok((response, exposed)),
            Ok(RegisteredControlResponse::RejectedBeforeDma(error)) => {
                drop(exposed.finished().map_err(rejected)?);
                Err(rejected(error))
            }
            Err(error @ Error::RequestRejected(_)) => {
                // ControlExecutor rejected the request locally before sending it.
                drop(exposed.finished().map_err(rejected)?);
                Err(unavailable(error))
            }
            Err(error) => Err(unknown(error)),
        }
    }
}

#[async_trait]
impl ObjectDataTransport for Transport {
    fn staged_upload(&self) -> Option<&dyn StagedUploadTransport> {
        Some(self)
    }

    async fn download_file(
        &self,
        request: DownloadRequest<'_>,
        file: Arc<std::fs::File>,
    ) -> DataTransportResult<Option<DownloadResult>> {
        self.download_into(request, download::Sink::File(file))
            .await
            .map(Some)
    }

    async fn download(
        &self,
        request: DownloadRequest<'_>,
        target: &mut (dyn AsyncWrite + Send + Unpin),
    ) -> DataTransportResult<DownloadResult> {
        self.download_into(request, download::Sink::Writer(target))
            .await
    }

    async fn upload(
        &self,
        request: UploadRequest<'_>,
        source: &mut (dyn AsyncRead + Unpin + Send),
    ) -> DataTransportResult<UploadResult> {
        self.upload_reader(request, source).await
    }
}

#[cfg(test)]
mod tests;
