use std::io;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::try_join_all;
use futures::{StreamExt, stream};
use rc_core::admin::{
    CapabilityApi, ClientDevnullRequest, ClientDevnullResult, DiagnosticApi, DiagnosticCapability,
};
use rc_core::{Error, Result};
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderMap, HeaderValue};
use reqwest::{Body, Method, Response, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::AdminClient;
use aws_sigv4::http_request::SignableBody;

pub(super) const CLIENT_DEVNULL_CHUNK_BYTES: usize = 64 * 1024;
const MAX_CLIENT_DEVNULL_RESPONSE_BYTES: usize = 64 * 1024;
static CLIENT_DEVNULL_ZERO_CHUNK: [u8; CLIENT_DEVNULL_CHUNK_BYTES] =
    [0; CLIENT_DEVNULL_CHUNK_BYTES];

#[derive(Debug, Deserialize)]
struct ClientDevnullResponse {
    kind: String,
    measured: bool,
    capability_note: Option<String>,
    rx_bytes: Option<u64>,
    duration_secs: Option<f64>,
    aggregate_write_throughput_bytes_per_sec: Option<f64>,
}

pub(super) fn next_client_devnull_chunk(remaining: u64) -> Option<(Bytes, u64)> {
    if remaining == 0 {
        return None;
    }

    let chunk_len = remaining.min(CLIENT_DEVNULL_CHUNK_BYTES as u64) as usize;
    Some((
        Bytes::from_static(&CLIENT_DEVNULL_ZERO_CHUNK[..chunk_len]),
        remaining - chunk_len as u64,
    ))
}

pub(super) fn client_devnull_payload_hash(bytes: u64) -> String {
    let mut hasher = Sha256::new();
    let mut remaining = bytes;
    while let Some((chunk, next_remaining)) = next_client_devnull_chunk(remaining) {
        hasher.update(&chunk);
        remaining = next_remaining;
    }
    hex::encode(hasher.finalize())
}

fn client_devnull_body(bytes: u64) -> Body {
    let chunks = stream::unfold(bytes, |remaining| async move {
        next_client_devnull_chunk(remaining)
            .map(|(chunk, next_remaining)| (Ok::<Bytes, io::Error>(chunk), next_remaining))
    });
    Body::wrap_stream(chunks)
}

impl AdminClient {
    fn client_devnull_headers(&self, bytes: u64, payload_hash: &str) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_LENGTH,
            HeaderValue::from_str(&bytes.to_string())
                .map_err(|error| Error::Auth(format!("Invalid content length header: {error}")))?,
        );
        headers.insert(
            "x-amz-content-sha256",
            HeaderValue::from_str(payload_hash)
                .map_err(|error| Error::Auth(format!("Invalid content hash header: {error}")))?,
        );
        headers.insert(
            HOST,
            HeaderValue::from_str(&self.get_host())
                .map_err(|error| Error::Auth(format!("Invalid host header: {error}")))?,
        );
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        Ok(headers)
    }

    async fn client_devnull_once(&self, bytes: u64, payload_hash: &str) -> Result<u64> {
        let method = Method::POST;
        let url = self.admin_url("/speedtest/client/devnull");
        let headers = self.client_devnull_headers(bytes, payload_hash)?;
        let signed_headers = self
            .sign_request_with_body(
                &method,
                &url,
                &headers,
                SignableBody::Precomputed(payload_hash.to_string()),
            )
            .await?;

        let response = self
            .http_client
            .request(method, url)
            .headers(signed_headers)
            .body(client_devnull_body(bytes))
            .send()
            .await
            .map_err(|error| Error::Network(format!("Client devnull request failed: {error}")))?;
        let (status, response_body) = self.read_client_devnull_response(response).await?;
        if !status.is_success() {
            return Err(self.map_error(status, &String::from_utf8_lossy(&response_body)));
        }

        let response: ClientDevnullResponse =
            serde_json::from_slice(&response_body).map_err(|error| {
                Error::UnsupportedFeature(format!(
                    "Client devnull response is not a valid measurement: {error}"
                ))
            })?;
        validate_client_devnull_response(response, bytes)
    }

    async fn read_client_devnull_response(
        &self,
        response: Response,
    ) -> Result<(StatusCode, Vec<u8>)> {
        let status = response.status();
        if response
            .content_length()
            .is_some_and(|length| length > MAX_CLIENT_DEVNULL_RESPONSE_BYTES as u64)
        {
            return Err(self.client_devnull_response_overflow(status));
        }

        let mut body = Vec::new();
        let mut chunks = response.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|error| {
                Error::Network(format!("Failed to read client devnull response: {error}"))
            })?;
            if body.len().saturating_add(chunk.len()) > MAX_CLIENT_DEVNULL_RESPONSE_BYTES {
                return Err(self.client_devnull_response_overflow(status));
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, body))
    }

    fn client_devnull_response_overflow(&self, status: StatusCode) -> Error {
        let message =
            format!("Client devnull response exceeded {MAX_CLIENT_DEVNULL_RESPONSE_BYTES} bytes");
        if status.is_success() {
            Error::UnsupportedFeature(message)
        } else {
            self.map_error(status, &message)
        }
    }

    async fn run_client_devnull(
        &self,
        request: ClientDevnullRequest,
    ) -> Result<ClientDevnullResult> {
        let payload_hash = client_devnull_payload_hash(request.bytes_per_request());
        let started = Instant::now();
        let requests = (0..request.concurrency())
            .map(|_| self.client_devnull_once(request.bytes_per_request(), payload_hash.as_str()));
        let received = tokio::time::timeout(request.timeout(), try_join_all(requests))
            .await
            .map_err(|_| {
                Error::Network(format!(
                    "Client devnull probe timed out after {} seconds",
                    request.timeout().as_secs()
                ))
            })??;
        let elapsed_seconds = started.elapsed().as_secs_f64().max(f64::MIN_POSITIVE);
        let received_bytes = received.into_iter().sum();

        Ok(ClientDevnullResult {
            requested_bytes: request.total_bytes(),
            received_bytes,
            concurrency: request.concurrency(),
            elapsed_seconds,
            aggregate_throughput_bytes_per_second: received_bytes as f64 / elapsed_seconds,
        })
    }
}

fn validate_client_devnull_response(
    response: ClientDevnullResponse,
    expected_bytes: u64,
) -> Result<u64> {
    let unsupported = |reason: String| {
        let note = response
            .capability_note
            .as_deref()
            .map(|note| format!(" ({note})"))
            .unwrap_or_default();
        Error::UnsupportedFeature(format!(
            "Client devnull measurement is unavailable: {reason}{note}"
        ))
    };

    if response.kind != "client-devnull" {
        return Err(unsupported(format!(
            "unexpected response kind '{}'",
            response.kind
        )));
    }
    if !response.measured {
        return Err(unsupported("server returned measured=false".to_string()));
    }
    let received_bytes = response
        .rx_bytes
        .ok_or_else(|| unsupported("rx_bytes is missing".to_string()))?;
    if received_bytes != expected_bytes {
        return Err(unsupported(format!(
            "server received {received_bytes} bytes, expected {expected_bytes}"
        )));
    }
    let duration = response
        .duration_secs
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| unsupported("duration_secs must be finite and positive".to_string()))?;
    let throughput = response
        .aggregate_write_throughput_bytes_per_sec
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| {
            unsupported(
                "aggregate_write_throughput_bytes_per_sec must be finite and positive".to_string(),
            )
        })?;
    let _validated_measurement = (duration, throughput);

    Ok(received_bytes)
}

#[async_trait]
impl DiagnosticApi for AdminClient {
    async fn client_devnull(&self, request: ClientDevnullRequest) -> Result<ClientDevnullResult> {
        let capabilities = self.discover_capabilities(false).await?;
        capabilities
            .require_diagnostic_capability(DiagnosticCapability::ClientDevnull)
            .map_err(|error| Error::UnsupportedFeature(error.to_string()))?;
        self.run_client_devnull(request).await
    }
}
