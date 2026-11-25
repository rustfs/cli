//! S3 object selection and native transport installation.

mod download;
mod upload;

use crate::wire::RDMA_REPLY_HEADER;
use async_trait::async_trait;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use rc_core::{DataTransferState, Error, ObjectInfo, RemotePath, Result};
use rc_s3::data_transport::*;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::pool::{Exposed, Pools, WINDOW, Window};
use crate::wire;
use rc_core::RdmaBackend;

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use tokio::sync::{Semaphore, SemaphorePermit};

/// A native transport factory for one backend. Reuse it across clients to share
/// endpoint capability probes. Each client owns its registered window pools.
pub struct RdmaProvider {
    backend: RdmaBackend,
    endpoints: Mutex<HashMap<String, Arc<EndpointSupport>>>,
}

impl RdmaProvider {
    /// Select a compiled backend. Registration is deferred until data transfer.
    pub fn new(backend: RdmaBackend) -> Result<Self> {
        let enabled = match backend {
            RdmaBackend::Nixl => cfg!(feature = "nixl"),
            RdmaBackend::CuObject => cfg!(feature = "cuobject"),
        };
        if !enabled {
            return Err(Error::UnsupportedFeature(format!(
                "RDMA backend {backend:?} is not enabled"
            )));
        }
        Ok(Self {
            backend,
            endpoints: Mutex::default(),
        })
    }

    /// Build an S3 client using this provider and the caller's transfer mode.
    pub async fn client(
        &self,
        alias: rc_core::Alias,
        mode: rc_core::DataTransportMode,
    ) -> Result<rc_s3::S3Client> {
        rc_s3::S3Client::new_with_transport_factory(alias, mode, self).await
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

impl ObjectDataTransportFactory for RdmaProvider {
    fn create(&self, control: Arc<dyn ControlExecutor>) -> Result<Arc<dyn ObjectDataTransport>> {
        Ok(Arc::new(Transport {
            support: self.support(control.endpoint()),
            control,
            pools: Pools::new(self.backend),
        }))
    }
}

struct Transport {
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
        request: ControlRequest,
        exposed: Exposed,
    ) -> DataTransportResult<(ControlResponse, Exposed)> {
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
