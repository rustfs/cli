//! S3 Select (`SelectObjectContent`) — AWS SDK mapping and streaming.

use aws_sdk_s3::types::{
    CompressionType, CsvInput, CsvOutput, ExpressionType, FileHeaderInfo, InputSerialization,
    JsonInput, JsonOutput, JsonType, OutputSerialization, ParquetInput, QuoteFields,
    SelectObjectContentEventStream,
};
use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
use aws_smithy_runtime_api::client::result::SdkError;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use aws_smithy_types::event_stream::RawMessage;
use rc_core::{
    Error, RemotePath, Result, SelectCompression, SelectInputFormat, SelectOptions,
    SelectOutputFormat,
};
use tokio::io::{AsyncWrite, AsyncWriteExt};

/// Run S3 Select and write record payloads to `writer` incrementally.
pub async fn select_object_content(
    client: &aws_sdk_s3::Client,
    path: &RemotePath,
    options: &SelectOptions,
    writer: &mut (dyn AsyncWrite + Send + Unpin),
) -> Result<()> {
    let input = build_input_serialization(options);
    let output = build_output_serialization(options);

    // aws-sdk-s3 `SelectObjectContent` does not expose object `VersionId`; the current object is used.
    let resp = client
        .select_object_content()
        .bucket(&path.bucket)
        .key(&path.key)
        .expression(&options.expression)
        .expression_type(ExpressionType::Sql)
        .input_serialization(input)
        .output_serialization(output)
        .send()
        .await
        .map_err(map_select_initial_error)?;

    let mut events = resp.payload;
    while let Some(ev) = events.recv().await.map_err(map_select_stream_error)? {
        match ev {
            SelectObjectContentEventStream::Records(rec) => {
                if let Some(blob) = rec.payload {
                    writer.write_all(blob.as_ref()).await.map_err(Error::Io)?;
                }
            }
            SelectObjectContentEventStream::End(_) => break,
            _ => {}
        }
    }
    writer.flush().await.map_err(Error::Io)?;
    Ok(())
}

/// Probe whether the bucket supports `SelectObjectContent` (lightweight; uses a non-existent key).
pub async fn probe_select_support(client: &aws_sdk_s3::Client, bucket: &str) -> Result<bool> {
    let probe_path = RemotePath::new("_", bucket, "__rc_select_probe__/object-does-not-exist");
    let opts = SelectOptions {
        expression: "SELECT s._1 FROM S3Object s LIMIT 0".to_string(),
        input_format: SelectInputFormat::Csv,
        output_format: SelectOutputFormat::Csv,
        compression: SelectCompression::None,
    };
    let mut sink = tokio::io::sink();
    match select_object_content(client, &probe_path, &opts, &mut sink).await {
        Ok(()) => Ok(true),
        Err(e) => classify_probe_or_pass_through(e),
    }
}

fn classify_probe_or_pass_through(err: Error) -> Result<bool> {
    match err {
        Error::UnsupportedFeature(_) => Ok(false),
        Error::NotFound(msg) => {
            if msg.to_ascii_lowercase().contains("bucket") {
                Err(Error::NotFound(msg))
            } else {
                // NoSuchKey: Select was accepted; object is missing.
                Ok(true)
            }
        }
        Error::Network(ref msg) if probe_network_implies_unsupported(msg) => Ok(false),
        other => Err(other),
    }
}

/// Narrow fallback when [`classify_aws_code`] could not classify (missing `x-amz-error-code`).
fn probe_network_implies_unsupported(msg: &str) -> bool {
    msg.contains("(code: NotImplemented)") || msg.contains("code: NotImplemented")
}

fn compression_type(c: SelectCompression) -> CompressionType {
    match c {
        SelectCompression::None => CompressionType::None,
        SelectCompression::Gzip => CompressionType::Gzip,
        SelectCompression::Bzip2 => CompressionType::Bzip2,
    }
}

fn build_input_serialization(options: &SelectOptions) -> InputSerialization {
    let compression = compression_type(options.compression);
    let mut b = InputSerialization::builder().compression_type(compression);
    match options.input_format {
        SelectInputFormat::Csv => {
            let csv = CsvInput::builder()
                .file_header_info(FileHeaderInfo::None)
                .build();
            b = b.csv(csv);
        }
        SelectInputFormat::Json => {
            // JSONL: one JSON object per line (S3 Select `Type=LINES`).
            let json = JsonInput::builder().r#type(JsonType::Lines).build();
            b = b.json(json);
        }
        SelectInputFormat::Parquet => {
            let pq = ParquetInput::builder().build();
            b = b.parquet(pq);
        }
    }
    b.build()
}

fn build_output_serialization(options: &SelectOptions) -> OutputSerialization {
    let mut b = OutputSerialization::builder();
    match options.output_format {
        SelectOutputFormat::Csv => {
            let csv = CsvOutput::builder()
                .quote_fields(QuoteFields::Asneeded)
                .build();
            b = b.csv(csv);
        }
        SelectOutputFormat::Json => {
            let json = JsonOutput::builder().build();
            b = b.json(json);
        }
    }
    b.build()
}

fn resolve_http_service_error_code<'a, E: ProvideErrorMetadata + ?Sized>(
    op_err: &'a E,
    raw: &'a HttpResponse,
) -> Option<&'a str> {
    op_err
        .code()
        .or_else(|| op_err.meta().code())
        .or_else(|| header_amz_error_code(raw))
}

fn header_amz_error_code(raw: &HttpResponse) -> Option<&str> {
    raw.headers().get("x-amz-error-code")
}

fn resolve_event_stream_error_code<'a, E: ProvideErrorMetadata + ?Sized>(
    op_err: &'a E,
    _raw: &'a RawMessage,
) -> Option<&'a str> {
    op_err.code().or_else(|| op_err.meta().code())
}

fn map_select_initial_error(
    err: SdkError<
        aws_sdk_s3::operation::select_object_content::SelectObjectContentError,
        HttpResponse,
    >,
) -> Error {
    use aws_sdk_s3::error::SdkError;
    match &err {
        SdkError::ServiceError(se) => {
            let code = resolve_http_service_error_code(se.err(), se.raw());
            classify_aws_code(code, &err.to_string())
        }
        SdkError::TimeoutError(_) => Error::Network("Request timeout".to_string()),
        SdkError::DispatchFailure(e) => Error::Network(format!("Network dispatch error: {e:?}")),
        SdkError::ResponseError(e) => Error::Network(format!("Response error: {e:?}")),
        SdkError::ConstructionFailure(e) => Error::General(format!("Request construction: {e:?}")),
        _ => Error::Network(err.to_string()),
    }
}

fn map_select_stream_error(
    err: SdkError<aws_sdk_s3::types::error::SelectObjectContentEventStreamError, RawMessage>,
) -> Error {
    use aws_sdk_s3::error::SdkError;
    match &err {
        SdkError::ServiceError(se) => {
            let code = resolve_event_stream_error_code(se.err(), se.raw());
            classify_aws_code(code, &err.to_string())
        }
        SdkError::TimeoutError(_) => Error::Network("Request timeout".to_string()),
        SdkError::DispatchFailure(e) => Error::Network(format!("Network dispatch error: {e:?}")),
        SdkError::ResponseError(e) => Error::Network(format!("Response error: {e:?}")),
        SdkError::ConstructionFailure(e) => Error::General(format!("Stream construction: {e:?}")),
        _ => Error::Network(err.to_string()),
    }
}

fn classify_aws_code(code: Option<&str>, text: &str) -> Error {
    let c = code.filter(|s| !s.is_empty());
    match c {
        Some("NoSuchKey") => Error::NotFound("Object not found".to_string()),
        Some("NoSuchBucket") => Error::NotFound("Bucket not found".to_string()),
        Some("AccessDenied") => Error::Auth("Access denied".to_string()),
        Some("NotImplemented") => Error::UnsupportedFeature(
            "The backend does not support S3 Select. Use --force to attempt anyway.".to_string(),
        ),
        Some("InvalidArgument") => Error::General(format!("Invalid S3 Select request: {text}")),
        Some(_) if text.contains("NotImplemented") => Error::UnsupportedFeature(
            "The backend does not support S3 Select. Use --force to attempt anyway.".to_string(),
        ),
        Some(_) => Error::Network(text.to_string()),
        None => classify_aws_code_missing_metadata(text),
    }
}

/// When the SDK did not surface `x-amz-error-code` / metadata, use minimal substring checks.
fn classify_aws_code_missing_metadata(text: &str) -> Error {
    if text.contains("NotImplemented") {
        return Error::UnsupportedFeature(
            "The backend does not support S3 Select. Use --force to attempt anyway.".to_string(),
        );
    }
    if text.contains("NoSuchKey") {
        return Error::NotFound("Object not found".to_string());
    }
    if text.contains("NoSuchBucket") {
        return Error::NotFound("Bucket not found".to_string());
    }
    Error::Network(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::classify_aws_code;
    use rc_core::Error;

    #[test]
    fn classify_maps_no_such_key() {
        let e = classify_aws_code(Some("NoSuchKey"), "");
        assert!(matches!(e, Error::NotFound(_)));
    }

    #[test]
    fn classify_maps_not_implemented() {
        let e = classify_aws_code(Some("NotImplemented"), "");
        assert!(matches!(e, Error::UnsupportedFeature(_)));
    }

    #[test]
    fn classify_fallback_network() {
        let e = classify_aws_code(Some("SlowDown"), "rate limited");
        assert!(matches!(e, Error::Network(_)));
    }

    #[test]
    fn classify_missing_code_maps_no_such_bucket_substring() {
        let e = classify_aws_code(None, "Service error: ... NoSuchBucket ...");
        assert!(matches!(e, Error::NotFound(msg) if msg.contains("Bucket")));
    }

    #[test]
    fn classify_maps_invalid_argument() {
        let e = classify_aws_code(Some("InvalidArgument"), "bad expr");
        assert!(matches!(e, Error::General(_)));
    }
}

#[cfg(test)]
mod probe_tests {
    use super::classify_probe_or_pass_through;
    use rc_core::Error;

    #[test]
    fn probe_pass_through_bucket_missing() {
        let err = Error::NotFound("Bucket not found".to_string());
        let out = classify_probe_or_pass_through(err);
        assert!(matches!(out, Err(Error::NotFound(_))));
    }

    #[test]
    fn probe_object_missing_means_select_accepted() {
        let err = Error::NotFound("Object not found".to_string());
        assert!(matches!(classify_probe_or_pass_through(err), Ok(true)));
    }

    #[test]
    fn probe_unsupported_feature() {
        let err = Error::UnsupportedFeature("no".to_string());
        assert!(matches!(classify_probe_or_pass_through(err), Ok(false)));
    }

    #[test]
    fn probe_network_not_implemented_code() {
        let err = Error::Network("Service error: ... (code: NotImplemented)".to_string());
        assert!(matches!(classify_probe_or_pass_through(err), Ok(false)));
    }
}
