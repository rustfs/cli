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

pub(crate) struct Factory(pub(crate) RdmaBackend);

impl ObjectDataTransportFactory for Factory {
    fn create(&self, control: Arc<dyn ControlExecutor>) -> Result<Arc<dyn ObjectDataTransport>> {
        Ok(Arc::new(Transport {
            control,
            pools: Pools::new(self.0),
        }))
    }
}

struct Transport {
    control: Arc<dyn ControlExecutor>,
    pools: Pools,
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
                drop(exposed.finished());
                Err(rejected(error))
            }
            Err(error @ Error::RequestRejected(_)) => {
                // ControlExecutor rejected the request locally before sending it.
                drop(exposed.finished());
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
