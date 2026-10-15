//! Transport-neutral ports for object payload delivery.
//!
//! The S3 client owns request policy, credentials, management operations, and
//! atomic file replacement. An injected provider owns only external payload IO.

use std::sync::{Arc, OnceLock};
use std::{
    pin::Pin,
    task::{Context, Poll},
};

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use rc_core::{
    DataTransferState, Error, ObjectInfo, ObjectWriteOptions, RemotePath, Result,
    TransferReadOptions,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A bounded signed control exchange. It is executed once without HTTP retries.
pub struct ControlRequest {
    pub method: Method,
    pub path: RemotePath,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub response_limit: usize,
}

/// Uninterpreted control response; duplicate and binary headers are preserved.
pub struct ControlResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Host-owned HTTPS, credentials, signing, custom headers, and TLS configuration.
#[async_trait]
pub trait ControlExecutor: Send + Sync {
    async fn execute(&self, request: ControlRequest) -> Result<ControlResponse>;
}

/// Configuration passed to a process-installed provider factory.
pub struct DataTransportContext {
    pub endpoint: String,
    pub control: Arc<dyn ControlExecutor>,
}

/// External transport construction is separate from process-wide policy.
pub trait ObjectDataTransportFactory: Send + Sync {
    fn create(&self, context: DataTransportContext) -> Result<Arc<dyn ObjectDataTransport>>;
}

static FACTORY: OnceLock<Arc<dyn ObjectDataTransportFactory>> = OnceLock::new();

/// Install a provider before running the CLI. Replacing a provider is rejected.
pub fn install_object_data_transport_factory(
    factory: Arc<dyn ObjectDataTransportFactory>,
) -> Result<()> {
    FACTORY.set(factory).map_err(|_| {
        Error::Config("An object data transport factory is already installed".to_string())
    })
}

pub(crate) fn installed_factory() -> Option<Arc<dyn ObjectDataTransportFactory>> {
    FACTORY.get().cloned()
}

/// A GET's S3 selection and an independent finite receive bound.
pub struct DownloadRequest<'a> {
    pub path: &'a RemotePath,
    pub options: &'a TransferReadOptions,
    /// Business Range selection; `None` requests the whole object.
    pub max_bytes: Option<u64>,
    /// Safety bound checked against the authenticated selected-object length.
    pub max_receive_bytes: u64,
    pub on_progress: &'a mut (dyn FnMut(u64, Option<u64>) + Send),
}

/// Verified logical completion, including the selected S3 result metadata.
pub struct DownloadResult {
    pub bytes_transferred: u64,
    pub content_length: u64,
}

/// S3 PUT conditions, independent of the external data protocol.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WriteCondition<'a> {
    #[default]
    None,
    IfAbsent,
    IfMatch(&'a str),
}

/// Object write or one part of an existing HTTP-managed multipart upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadOperation<'a> {
    PutObject {
        condition: WriteCondition<'a>,
    },
    UploadPart {
        upload_id: &'a str,
        part_number: i32,
    },
}

/// Known-length upload. All S3 options remain available to the provider.
pub struct UploadRequest<'a> {
    pub path: &'a RemotePath,
    pub operation: UploadOperation<'a>,
    pub options: &'a ObjectWriteOptions,
    pub content_length: u64,
    /// The checksum already selected/calculated by the S3 operation planner.
    pub checksum_sha256: Option<&'a str>,
}

/// Authoritative S3 result after payload verification and business completion.
pub struct UploadResult {
    pub object: ObjectInfo,
    pub checksum_sha256: Option<String>,
}

/// A typed failure never eligible for generic timeout-based business retries.
#[derive(Debug, thiserror::Error)]
#[error("Object data transfer {state}: {message}")]
pub struct DataTransportError {
    pub state: DataTransferState,
    /// Safe diagnostics must exclude credentials, opaque offers, and raw bodies.
    pub message: String,
    /// Authoritative S3 error kept separate from replay classification.
    pub source: Option<Box<Error>>,
}

impl From<DataTransportError> for Error {
    fn from(error: DataTransportError) -> Self {
        Self::DataTransfer {
            state: error.state,
            message: error.message,
            source: error.source,
        }
    }
}

pub type DataTransportResult<T> = std::result::Result<T, DataTransportError>;

pub(crate) fn committed_verification_failure(source: Error) -> Error {
    Error::DataTransfer {
        state: DataTransferState::ConfirmedFailure,
        message: "The object was committed but its post-write verification failed".to_string(),
        source: Some(Box::new(source)),
    }
}

/// One provider call performs one S3 data operation and its owned reconciliation.
///
/// Success requires authenticated acceptance, exact logical length, data
/// verification, and an authoritative S3 result. `NotAccepted` guarantees that
/// neither supplied IO was polled nor any request accepted. All other failures
/// retain their classification; callers never infer replay safety from strings.
#[async_trait]
pub trait ObjectDataTransport: Send + Sync {
    async fn download(
        &self,
        request: DownloadRequest<'_>,
        target: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> DataTransportResult<DownloadResult>;

    async fn upload(
        &self,
        request: UploadRequest<'_>,
        source: &mut (dyn AsyncRead + Unpin + Send),
    ) -> DataTransportResult<UploadResult>;
}

pub(crate) struct ObservedWriter<'a, W: ?Sized> {
    pub inner: &'a mut W,
    pub polled: bool,
    pub bytes: u64,
    pub limit: u64,
    pub flushed: bool,
}

impl<W: AsyncWrite + Unpin + ?Sized> AsyncWrite for ObservedWriter<'_, W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.polled = true;
        if bytes.len() as u64 > self.limit.saturating_sub(self.bytes) {
            return Poll::Ready(Err(std::io::Error::other(
                "External download exceeds the receive bound",
            )));
        }
        match Pin::new(&mut *self.inner).poll_write(cx, bytes) {
            Poll::Ready(Ok(written)) => {
                self.bytes += written as u64;
                self.flushed = false;
                Poll::Ready(Ok(written))
            }
            result => result,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.polled = true;
        let result = Pin::new(&mut *self.inner).poll_flush(cx);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.flushed = true;
        }
        result
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Err(std::io::Error::other(
            "External downloads must not shut down the caller's writer",
        )))
    }
}

pub(crate) struct ObservedReader<'a, R: ?Sized> {
    pub inner: &'a mut R,
    pub polled: bool,
    pub bytes: u64,
    pub eof: bool,
}

impl<R: AsyncRead + Unpin + ?Sized> AsyncRead for ObservedReader<'_, R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.polled = true;
        let previous = buffer.filled().len();
        let had_capacity = buffer.remaining() > 0;
        match Pin::new(&mut *self.inner).poll_read(cx, buffer) {
            Poll::Ready(Ok(())) => {
                let read = buffer.filled().len() - previous;
                self.bytes += read as u64;
                self.eof |= read == 0 && had_capacity;
                Poll::Ready(Ok(()))
            }
            result => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_classification_survives_conversion() {
        let error: Error = DataTransportError {
            state: DataTransferState::OutcomeUnknown,
            message: "timeout".to_string(),
            source: None,
        }
        .into();
        assert!(matches!(
            error,
            Error::DataTransfer {
                state: DataTransferState::OutcomeUnknown,
                ..
            }
        ));
        assert!(!rc_core::is_retryable_error(&error));
    }
}
