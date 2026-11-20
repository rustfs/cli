//! S3 object selection and native transport installation.

mod download;
mod upload;

use async_trait::async_trait;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use rc_core::{DataTransferState, Error, ObjectInfo, RemotePath, Result};
use rc_s3::data_transport::*;
use rustfs_nixl::{RDMA_REPLY_HEADER, StagingChunk};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::OnceCell;

use crate::pool::{Exposed, Pool, SMALL_WINDOW, WINDOW, native_error};
use crate::wire;

pub(crate) struct Factory;

impl ObjectDataTransportFactory for Factory {
    fn create(&self, context: DataTransportContext) -> Result<Arc<dyn ObjectDataTransport>> {
        Ok(Arc::new(Transport {
            control: context.control,
            small_pool: OnceCell::new(),
            full_pool: OnceCell::new(),
        }))
    }
}

struct Transport {
    control: Arc<dyn ControlExecutor>,
    small_pool: OnceCell<Arc<Pool>>,
    full_pool: OnceCell<Arc<Pool>>,
}

fn failed(state: DataTransferState, source: Error) -> DataTransportError {
    DataTransportError {
        state,
        message: "RustFS NIXL object transfer".into(),
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
        match self.control.execute_headers(request).await {
            Ok(response) => Ok((response, exposed)),
            Err(error @ Error::RequestRejected(_)) => {
                // ControlExecutor rejected the request locally before sending it.
                drop(exposed.finished());
                Err(unavailable(error))
            }
            Err(error) => Err(unknown(error)),
        }
    }

    async fn pool(&self, required: u64) -> DataTransportResult<&Arc<Pool>> {
        if let Some(pool) = self.full_pool.get() {
            return Ok(pool);
        }
        let (slot, window) = if required <= SMALL_WINDOW as u64 {
            (&self.small_pool, SMALL_WINDOW)
        } else {
            (&self.full_pool, WINDOW)
        };
        slot.get_or_try_init(|| async {
            tokio::task::spawn_blocking(move || Pool::open(window))
                .await
                .map_err(|_| Error::General("NIXL agent startup was interrupted".into()))?
        })
        .await
        .map_err(unavailable)
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
