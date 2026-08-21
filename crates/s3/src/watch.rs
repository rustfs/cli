//! RustFS live notification streaming transport.

use std::collections::{HashMap, VecDeque};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt as _, stream};
use rc_core::{
    Error, Result, WatchApi, WatchEvent, WatchFrame, WatchRequest, WatchSource, WatchStream,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HOST, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::Deserialize;

use crate::S3Client;

const EVENT_STREAM_CONTENT_TYPE: &str = "text/event-stream";
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_WATCH_LINE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct NotificationEnvelope {
    #[serde(rename = "Records")]
    records: Vec<RawNotificationEvent>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawNotificationEvent {
    event_source: Option<String>,
    aws_region: Option<String>,
    event_time: String,
    event_name: String,
    user_identity: Option<RawIdentity>,
    #[serde(default)]
    response_elements: HashMap<String, String>,
    s3: RawS3Metadata,
    source: Option<RawSource>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawIdentity {
    principal_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawS3Metadata {
    bucket: RawBucket,
    object: RawObject,
}

#[derive(Debug, Deserialize)]
struct RawBucket {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawObject {
    key: String,
    size: Option<i64>,
    e_tag: Option<String>,
    version_id: Option<String>,
    sequencer: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSource {
    host: Option<String>,
    port: Option<String>,
    user_agent: Option<String>,
}

struct DecoderState {
    upstream: WatchByteStream,
    buffer: Vec<u8>,
    pending: VecDeque<WatchFrame>,
    eof: bool,
}

type WatchByteStream = std::pin::Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct S3ErrorBody {
    code: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JsonErrorBody {
    #[serde(alias = "Code")]
    code: Option<String>,
    #[serde(alias = "Message")]
    message: Option<String>,
    #[serde(alias = "Error")]
    error: Option<String>,
}

impl S3Client {
    fn watch_url(&self, request: &WatchRequest) -> Result<reqwest::Url> {
        if request.events.is_empty() {
            return Err(Error::InvalidPath(
                "At least one watch event is required".to_string(),
            ));
        }
        if request.ping_seconds == 0 {
            return Err(Error::InvalidPath(
                "Watch ping interval must be greater than zero".to_string(),
            ));
        }

        let mut url = reqwest::Url::parse(self.watch_alias().endpoint.trim_end_matches('/'))
            .map_err(|error| {
                Error::Network(format!(
                    "Invalid endpoint '{}': {error}",
                    self.watch_alias().endpoint
                ))
            })?;

        if let Some(bucket) = &request.bucket {
            let mut segments = url.path_segments_mut().map_err(|_| {
                Error::Network(format!(
                    "Endpoint '{}' does not support path-style watch operations",
                    self.watch_alias().endpoint
                ))
            })?;
            segments.pop_if_empty();
            segments.push(bucket);
        } else if url.path().is_empty() {
            url.set_path("/");
        }

        {
            let mut query = url.query_pairs_mut();
            for event in &request.events {
                query.append_pair("events", event);
            }
            if let Some(prefix) = &request.prefix {
                query.append_pair("prefix", prefix);
            }
            if let Some(suffix) = &request.suffix {
                query.append_pair("suffix", suffix);
            }
            query.append_pair("ping", &request.ping_seconds.to_string());
        }

        Ok(url)
    }

    async fn signed_watch_headers(&self, url: &reqwest::Url) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amz-content-sha256",
            HeaderValue::from_static(
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
        );
        headers.insert(
            HOST,
            HeaderValue::from_str(&self.watch_request_host(url)?)
                .map_err(|error| Error::Auth(format!("Invalid host header: {error}")))?,
        );
        headers.insert(ACCEPT, HeaderValue::from_static(EVENT_STREAM_CONTENT_TYPE));

        for header in self.watch_request_headers() {
            let name = HeaderName::from_bytes(header.name.as_bytes())
                .map_err(|error| Error::Auth(format!("Invalid custom header name: {error}")))?;
            let value = HeaderValue::from_str(&header.value)
                .map_err(|error| Error::Auth(format!("Invalid custom header value: {error}")))?;
            headers.insert(name, value);
        }

        self.sign_watch_request(&Method::GET, url.as_str(), &headers)
            .await
    }

    async fn open_watch_stream(&self, request: &WatchRequest) -> Result<WatchStream> {
        let url = self.watch_url(request)?;
        let headers = self.signed_watch_headers(&url).await?;
        let response = self
            .watch_http_client()
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|error| Error::Network(format!("Failed to open watch stream: {error}")))?;

        if !response.status().is_success() {
            return Err(watch_http_error(response).await);
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type
            .to_ascii_lowercase()
            .starts_with(EVENT_STREAM_CONTENT_TYPE)
        {
            return Err(Error::General(
                "Watch endpoint returned an unexpected content type".to_string(),
            ));
        }

        let upstream = response.bytes_stream().map(|result| {
            result.map_err(|error| Error::Network(format!("Watch stream read failed: {error}")))
        });
        Ok(decode_watch_stream(Box::pin(upstream)))
    }
}

#[async_trait]
impl WatchApi for S3Client {
    async fn watch(&self, request: &WatchRequest) -> Result<WatchStream> {
        self.open_watch_stream(request).await
    }
}

fn decode_watch_stream(upstream: WatchByteStream) -> WatchStream {
    let state = DecoderState {
        upstream,
        buffer: Vec::new(),
        pending: VecDeque::new(),
        eof: false,
    };

    Box::pin(stream::try_unfold(state, |mut state| async move {
        loop {
            if let Some(frame) = state.pending.pop_front() {
                return Ok(Some((frame, state)));
            }

            if let Some(newline) = state.buffer.iter().position(|byte| *byte == b'\n') {
                if newline > MAX_WATCH_LINE_BYTES {
                    return Err(Error::General(
                        "Watch event exceeds the maximum supported line size".to_string(),
                    ));
                }
                let mut line = state.buffer.drain(..=newline).collect::<Vec<_>>();
                line.pop();
                decode_line(&line, &mut state.pending)?;
                continue;
            }

            if state.buffer.len() > MAX_WATCH_LINE_BYTES {
                return Err(Error::General(
                    "Watch event exceeds the maximum supported line size".to_string(),
                ));
            }

            if state.eof {
                if state.buffer.is_empty() {
                    return Ok(None);
                }
                let line = std::mem::take(&mut state.buffer);
                decode_line(&line, &mut state.pending)?;
                continue;
            }

            match state.upstream.next().await {
                Some(Ok(bytes)) => state.buffer.extend_from_slice(&bytes),
                Some(Err(error)) => return Err(error),
                None => state.eof = true,
            }
        }
    }))
}

fn decode_line(line: &[u8], frames: &mut VecDeque<WatchFrame>) -> Result<()> {
    let payload = trim_ascii_whitespace(line);
    if payload.is_empty() {
        frames.push_back(WatchFrame::KeepAlive);
        return Ok(());
    }

    let envelope: NotificationEnvelope = serde_json::from_slice(payload)?;
    if envelope.records.is_empty() {
        frames.push_back(WatchFrame::KeepAlive);
        return Ok(());
    }

    for record in envelope.records {
        frames.push_back(WatchFrame::Event(Box::new(normalize_event(record)?)));
    }
    Ok(())
}

fn normalize_event(record: RawNotificationEvent) -> Result<WatchEvent> {
    if record.s3.object.size.is_some_and(|size| size < 0) {
        return Err(Error::General(
            "Malformed watch event: object size cannot be negative".to_string(),
        ));
    }

    let event_time = record
        .event_time
        .parse()
        .map_err(|_| Error::General("Malformed watch event: invalid eventTime".to_string()))?;
    let key = urlencoding::decode(&record.s3.object.key)
        .map(|decoded| decoded.into_owned())
        .unwrap_or(record.s3.object.key);
    let event_id = non_empty(record.response_elements.get("x-amz-request-id").cloned())
        .or_else(|| non_empty(record.s3.object.sequencer));

    let raw_source = record.source.unwrap_or(RawSource {
        host: None,
        port: None,
        user_agent: None,
    });
    let source = WatchSource {
        event_source: non_empty(record.event_source),
        region: non_empty(record.aws_region),
        principal_id: record
            .user_identity
            .and_then(|identity| non_empty(identity.principal_id)),
        host: non_empty(raw_source.host),
        port: non_empty(raw_source.port),
        user_agent: non_empty(raw_source.user_agent),
    };

    Ok(WatchEvent {
        event_id,
        delete_marker: record.event_name.ends_with(":DeleteMarkerCreated"),
        event_name: record.event_name,
        bucket: record.s3.bucket.name,
        key,
        version_id: non_empty(record.s3.object.version_id),
        size_bytes: record.s3.object.size,
        etag: non_empty(record.s3.object.e_tag),
        event_time,
        source: (!source.is_empty()).then_some(source),
    })
}

fn trim_ascii_whitespace(input: &[u8]) -> &[u8] {
    let start = input
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(input.len());
    let end = input
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |position| position + 1);
    &input[start..end]
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

async fn watch_http_error(response: reqwest::Response) -> Error {
    let status = response.status();
    let body = read_bounded_error_body(response).await;
    map_watch_http_error(status, &body)
}

async fn read_bounded_error_body(response: reqwest::Response) -> Vec<u8> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(body.len());
        if remaining == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    body
}

fn map_watch_http_error(status: StatusCode, body: &[u8]) -> Error {
    let detail = watch_error_detail(body);
    // Some S3-compatible servers return a plain-text authentication failure. Use the
    // bounded body for classification, but only include parsed and sanitized fields in
    // user-facing output so arbitrary proxy pages are not reflected into the terminal.
    let classification_text = String::from_utf8_lossy(body).to_ascii_lowercase();
    let detail = detail
        .map(|detail| format!(": {detail}"))
        .unwrap_or_default();
    let message = format!("Watch request failed with HTTP {}{detail}", status.as_u16());

    match status {
        StatusCode::BAD_REQUEST
            if [
                "accessdenied",
                "invalidaccesskeyid",
                "signaturedoesnotmatch",
                "expiredtoken",
                "get cred failed",
            ]
            .iter()
            .any(|marker| classification_text.contains(marker)) =>
        {
            Error::Auth(message)
        }
        StatusCode::BAD_REQUEST if classification_text.contains("notimplemented") => {
            Error::UnsupportedFeature(message)
        }
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => Error::InvalidPath(message),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Error::Auth(message),
        StatusCode::NOT_FOUND if classification_text.contains("nosuchbucket") => {
            Error::NotFound(message)
        }
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED => {
            Error::UnsupportedFeature(message)
        }
        StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => Error::Conflict(message),
        StatusCode::REQUEST_TIMEOUT
        | StatusCode::TOO_MANY_REQUESTS
        | StatusCode::BAD_GATEWAY
        | StatusCode::SERVICE_UNAVAILABLE
        | StatusCode::GATEWAY_TIMEOUT => Error::Network(message),
        status if status.is_server_error() => Error::Network(message),
        _ => Error::General(message),
    }
}

fn watch_error_detail(body: &[u8]) -> Option<String> {
    let (code, message) = if let Ok(error) = quick_xml::de::from_reader::<_, S3ErrorBody>(body) {
        (error.code, error.message)
    } else if let Ok(error) = serde_json::from_slice::<JsonErrorBody>(body) {
        (error.code.or(error.error), error.message)
    } else {
        return None;
    };

    let code = code.and_then(safe_error_component);
    let message = message.and_then(safe_error_component);
    match (code, message) {
        (Some(code), Some(message)) => Some(format!("{code}: {message}")),
        (Some(code), None) => Some(code),
        (None, Some(message)) => Some(message),
        (None, None) => None,
    }
}

fn safe_error_component(value: String) -> Option<String> {
    let safe = value
        .chars()
        .take(512)
        .flat_map(char::escape_default)
        .collect::<String>();
    (!safe.is_empty()).then_some(safe)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use futures::stream;
    use rc_core::{Alias, WatchFrame};

    use super::*;

    const PUT_EVENT: &str = concat!(
        r#"{"Records":[{"eventVersion":"2.0","eventSource":"rustfs:s3","awsRegion":"us-east-1","eventTime":"2026-07-21T04:00:00.000Z","eventName":"s3:ObjectCreated:Put","userIdentity":{"principalId":"rustfs"},"requestParameters":{},"responseElements":{"x-amz-request-id":"req-1"},"s3":{"s3SchemaVersion":"1.0","configurationId":"Config","bucket":{"name":"photos","ownerIdentity":{"principalId":"rustfs"},"arn":"arn:aws:s3:::photos"},"object":{"key":"folder%2Fimage+copy.jpg","size":2048,"eTag":"abc123","versionId":"v1","sequencer":"001"}},"source":{"host":"node-1","port":"9000","userAgent":"test-agent"}}]}"#,
        "\n"
    );
    const DELETE_MARKER_EVENT: &str = concat!(
        r#"{"Records":[{"eventTime":"2026-07-21T04:01:00.000Z","eventName":"s3:ObjectRemoved:DeleteMarkerCreated","s3":{"bucket":{"name":"photos"},"object":{"key":"old%2Fimage.jpg","versionId":"delete-v1","sequencer":"002"}}}]}"#,
        "\n"
    );

    fn request(bucket: Option<&str>) -> WatchRequest {
        WatchRequest {
            bucket: bucket.map(str::to_string),
            events: vec![
                "s3:ObjectCreated:*".to_string(),
                "s3:ObjectRemoved:*".to_string(),
            ],
            prefix: Some("folder/".to_string()),
            suffix: Some(".jpg".to_string()),
            ping_seconds: 5,
        }
    }

    fn test_client(endpoint: &str) -> S3Client {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");
        runtime
            .block_on(S3Client::new(Alias::new(
                "local",
                endpoint,
                "access-key",
                "secret-key",
            )))
            .expect("client should build")
    }

    #[tokio::test]
    async fn decoder_handles_frames_split_across_http_chunks() {
        let split = PUT_EVENT.len() / 2;
        let chunks = vec![
            Ok(Bytes::copy_from_slice(&PUT_EVENT.as_bytes()[..split])),
            Ok(Bytes::copy_from_slice(&PUT_EVENT.as_bytes()[split..])),
        ];
        let mut decoded = decode_watch_stream(Box::pin(stream::iter(chunks)));

        let frame = decoded
            .next()
            .await
            .expect("event frame")
            .expect("valid frame");
        let WatchFrame::Event(event) = frame else {
            panic!("expected an event frame");
        };
        assert_eq!(event.key, "folder/image+copy.jpg");
        assert_eq!(event.event_id.as_deref(), Some("req-1"));
        assert_eq!(event.version_id.as_deref(), Some("v1"));
        assert!(!event.delete_marker);
        assert_eq!(
            event
                .source
                .as_ref()
                .and_then(|source| source.host.as_deref()),
            Some("node-1")
        );
        assert!(decoded.next().await.is_none());
    }

    #[tokio::test]
    async fn decoder_classifies_whitespace_and_empty_records_as_keepalives() {
        let chunks = vec![
            Ok(Bytes::from_static(b" \t\r\n")),
            Ok(Bytes::from_static(b"{\"Records\":[]}\n")),
        ];
        let frames = decode_watch_stream(Box::pin(stream::iter(chunks)))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(frames.len(), 2);
        assert!(
            frames
                .into_iter()
                .all(|frame| matches!(frame, Ok(WatchFrame::KeepAlive)))
        );
    }

    #[tokio::test]
    async fn decoder_preserves_versioned_delete_marker_events() {
        let chunks = vec![Ok(Bytes::from_static(DELETE_MARKER_EVENT.as_bytes()))];
        let mut decoded = decode_watch_stream(Box::pin(stream::iter(chunks)));

        let frame = decoded
            .next()
            .await
            .expect("event frame")
            .expect("valid frame");
        let WatchFrame::Event(event) = frame else {
            panic!("expected an event frame");
        };
        assert_eq!(event.event_id.as_deref(), Some("002"));
        assert_eq!(event.key, "old/image.jpg");
        assert_eq!(event.version_id.as_deref(), Some("delete-v1"));
        assert!(event.delete_marker);
        assert_eq!(event.size_bytes, None);
        assert!(decoded.next().await.is_none());
    }

    #[tokio::test]
    async fn decoder_surfaces_malformed_events_instead_of_discarding_them() {
        let chunks = vec![Ok(Bytes::from_static(
            b"{\"Records\":[{\"eventName\":\"s3:ObjectCreated:Put\"}]}\n",
        ))];
        let mut decoded = decode_watch_stream(Box::pin(stream::iter(chunks)));

        let error = decoded
            .next()
            .await
            .expect("decoder result")
            .expect_err("malformed record should fail");
        assert!(matches!(error, Error::Json(_)));
    }

    #[test]
    fn root_watch_url_repeats_event_filters_and_is_signed() {
        let client = test_client("https://example.com");
        let mut root_request = request(None);
        root_request.prefix = None;
        root_request.suffix = None;
        let url = client
            .watch_url(&root_request)
            .expect("root URL should build");
        assert_eq!(url.path(), "/");
        assert_eq!(
            url.query_pairs()
                .filter(|(key, _)| key == "events")
                .map(|(_, value)| value.into_owned())
                .collect::<Vec<_>>(),
            root_request.events
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");
        let headers = runtime
            .block_on(client.signed_watch_headers(&url))
            .expect("root request should sign");
        assert!(headers.contains_key("authorization"));
        assert_eq!(
            headers.get(HOST).and_then(|value| value.to_str().ok()),
            Some("example.com")
        );
    }

    #[test]
    fn bucket_watch_url_signs_encoded_prefix_suffix_and_ping_filters() {
        let client = test_client("https://example.com");
        let url = client
            .watch_url(&request(Some("photos")))
            .expect("bucket URL should build");
        let query = url.query_pairs().into_owned().collect::<HashMap<_, _>>();

        assert_eq!(url.path(), "/photos");
        assert_eq!(query.get("prefix").map(String::as_str), Some("folder/"));
        assert_eq!(query.get("suffix").map(String::as_str), Some(".jpg"));
        assert_eq!(query.get("ping").map(String::as_str), Some("5"));

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");
        let headers = runtime
            .block_on(client.signed_watch_headers(&url))
            .expect("bucket request should sign");
        assert!(headers.contains_key("authorization"));
    }

    #[test]
    fn structured_http_errors_preserve_exit_code_categories() {
        let denied = map_watch_http_error(
            StatusCode::FORBIDDEN,
            b"<Error><Code>AccessDenied</Code><Message>denied</Message></Error>",
        );
        let unavailable = map_watch_http_error(
            StatusCode::NOT_IMPLEMENTED,
            br#"{"code":"NotImplemented","message":"route unavailable"}"#,
        );
        let retryable = map_watch_http_error(StatusCode::SERVICE_UNAVAILABLE, b"");
        let missing_bucket = map_watch_http_error(
            StatusCode::NOT_FOUND,
            b"<Error><Code>NoSuchBucket</Code><Message>missing</Message></Error>",
        );
        let missing_bucket_json = map_watch_http_error(
            StatusCode::NOT_FOUND,
            br#"{"Code":"NoSuchBucket","Message":"missing"}"#,
        );
        let credentials = map_watch_http_error(StatusCode::BAD_REQUEST, b"get cred failed");

        assert!(matches!(denied, Error::Auth(_)));
        assert!(matches!(unavailable, Error::UnsupportedFeature(_)));
        assert!(matches!(retryable, Error::Network(_)));
        assert!(matches!(missing_bucket, Error::NotFound(_)));
        assert!(matches!(missing_bucket_json, Error::NotFound(_)));
        assert!(matches!(credentials, Error::Auth(_)));
        assert!(!denied.to_string().contains('\n'));
    }
}
