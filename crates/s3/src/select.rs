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
    let input = build_input_serialization(options)?;
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

fn compression_type(c: SelectCompression) -> CompressionType {
    match c {
        SelectCompression::None => CompressionType::None,
        SelectCompression::Gzip => CompressionType::Gzip,
        SelectCompression::Bzip2 => CompressionType::Bzip2,
    }
}

fn build_input_serialization(options: &SelectOptions) -> Result<InputSerialization> {
    if matches!(options.input_format, SelectInputFormat::Parquet)
        && !matches!(options.compression, SelectCompression::None)
    {
        return Err(Error::General(
            "Parquet input does not support whole-object GZIP or BZIP2 compression.".to_string(),
        ));
    }

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
    Ok(b.build())
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
        Some("NotImplemented") => {
            Error::UnsupportedFeature("The backend does not support S3 Select.".to_string())
        }
        Some("InvalidArgument") => Error::General(format!("Invalid S3 Select request: {text}")),
        Some(_) if text.contains("NotImplemented") => {
            Error::UnsupportedFeature("The backend does not support S3 Select.".to_string())
        }
        Some(_) => Error::General(text.to_string()),
        None => classify_aws_code_missing_metadata(text),
    }
}

/// When the SDK did not surface `x-amz-error-code` / metadata, use minimal substring checks.
fn classify_aws_code_missing_metadata(text: &str) -> Error {
    if text.contains("NotImplemented") {
        return Error::UnsupportedFeature("The backend does not support S3 Select.".to_string());
    }
    if text.contains("NoSuchKey") {
        return Error::NotFound("Object not found".to_string());
    }
    if text.contains("NoSuchBucket") {
        return Error::NotFound("Bucket not found".to_string());
    }
    if text.contains("AccessDenied") {
        return Error::Auth("Access denied".to_string());
    }
    Error::General(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::{build_input_serialization, build_output_serialization, classify_aws_code};
    use aws_sdk_s3::types::{CompressionType, FileHeaderInfo, JsonType, QuoteFields};
    use rc_core::Error;
    use rc_core::{SelectCompression, SelectInputFormat, SelectOptions, SelectOutputFormat};

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
        assert!(matches!(e, Error::General(_)));
    }

    #[test]
    fn classify_missing_code_maps_no_such_bucket_substring() {
        let e = classify_aws_code(None, "Service error: ... NoSuchBucket ...");
        assert!(matches!(e, Error::NotFound(msg) if msg.contains("Bucket")));
    }

    #[test]
    fn classify_missing_code_maps_no_such_key_substring() {
        let e = classify_aws_code(None, "Service error: ... NoSuchKey ...");
        assert!(matches!(e, Error::NotFound(msg) if msg.contains("Object")));
    }

    #[test]
    fn classify_missing_code_maps_access_denied_substring() {
        let e = classify_aws_code(None, "Service error: ... AccessDenied ...");
        assert!(matches!(e, Error::Auth(msg) if msg.contains("Access denied")));
    }

    #[test]
    fn classify_empty_code_maps_access_denied_substring() {
        let e = classify_aws_code(Some(""), "Service error: ... AccessDenied ...");
        assert!(matches!(e, Error::Auth(msg) if msg.contains("Access denied")));
    }

    #[test]
    fn classify_empty_code_maps_no_such_bucket_substring() {
        let e = classify_aws_code(Some(""), "Service error: ... NoSuchBucket ...");
        assert!(matches!(e, Error::NotFound(msg) if msg.contains("Bucket")));
    }

    #[test]
    fn classify_empty_code_maps_not_implemented_substring() {
        let e = classify_aws_code(Some(""), "Service error: backend returned NotImplemented");
        assert!(matches!(e, Error::UnsupportedFeature(msg) if msg.contains("does not support")));
    }

    #[test]
    fn classify_maps_invalid_argument() {
        let e = classify_aws_code(Some("InvalidArgument"), "bad expr");
        assert!(matches!(e, Error::General(_)));
    }

    #[test]
    fn classify_unknown_code_with_not_implemented_text_maps_unsupported_feature() {
        let e = classify_aws_code(Some("SlowDown"), "backend replied with NotImplemented");
        assert!(matches!(e, Error::UnsupportedFeature(msg) if msg.contains("does not support")));
    }

    #[test]
    fn classify_missing_code_unknown_maps_general() {
        let e = classify_aws_code(None, "Service error: query parsing failed");
        assert!(matches!(e, Error::General(_)));
    }

    #[test]
    fn classify_missing_code_maps_not_implemented_substring() {
        let e = classify_aws_code(None, "Service error: backend returned NotImplemented");
        assert!(matches!(e, Error::UnsupportedFeature(msg) if msg.contains("does not support")));
    }

    #[test]
    fn parquet_rejects_whole_object_compression() {
        let options = SelectOptions {
            expression: "SELECT * FROM S3Object".to_string(),
            input_format: SelectInputFormat::Parquet,
            output_format: SelectOutputFormat::Csv,
            compression: SelectCompression::Gzip,
        };

        let error = build_input_serialization(&options)
            .expect_err("parquet should reject whole-object compression");
        assert!(matches!(error, Error::General(_)));
    }

    #[test]
    fn parquet_allows_no_compression() {
        let options = SelectOptions {
            expression: "SELECT * FROM S3Object".to_string(),
            input_format: SelectInputFormat::Parquet,
            output_format: SelectOutputFormat::Csv,
            compression: SelectCompression::None,
        };

        build_input_serialization(&options).expect("parquet without whole-object compression");
    }

    #[test]
    fn csv_input_serialization_uses_csv_defaults_and_compression() {
        let options = SelectOptions {
            expression: "SELECT * FROM S3Object".to_string(),
            input_format: SelectInputFormat::Csv,
            output_format: SelectOutputFormat::Csv,
            compression: SelectCompression::Bzip2,
        };

        let input = build_input_serialization(&options).expect("csv input serialization");
        let csv = input.csv().expect("csv input is configured");

        assert_eq!(input.compression_type(), Some(&CompressionType::Bzip2));
        assert_eq!(csv.file_header_info(), Some(&FileHeaderInfo::None));
        assert!(input.json().is_none());
        assert!(input.parquet().is_none());
    }

    #[test]
    fn json_input_serialization_uses_lines_mode() {
        let options = SelectOptions {
            expression: "SELECT * FROM S3Object".to_string(),
            input_format: SelectInputFormat::Json,
            output_format: SelectOutputFormat::Json,
            compression: SelectCompression::Gzip,
        };

        let input = build_input_serialization(&options).expect("json input serialization");
        let json = input.json().expect("json input is configured");

        assert_eq!(input.compression_type(), Some(&CompressionType::Gzip));
        assert_eq!(json.r#type(), Some(&JsonType::Lines));
        assert!(input.csv().is_none());
        assert!(input.parquet().is_none());
    }

    #[test]
    fn output_serialization_selects_csv_or_json_shape() {
        let csv_options = SelectOptions {
            expression: "SELECT * FROM S3Object".to_string(),
            input_format: SelectInputFormat::Csv,
            output_format: SelectOutputFormat::Csv,
            compression: SelectCompression::None,
        };
        let csv_output = build_output_serialization(&csv_options);
        let csv = csv_output.csv().expect("csv output is configured");
        assert_eq!(csv.quote_fields(), Some(&QuoteFields::Asneeded));
        assert!(csv_output.json().is_none());

        let json_options = SelectOptions {
            expression: "SELECT * FROM S3Object".to_string(),
            input_format: SelectInputFormat::Json,
            output_format: SelectOutputFormat::Json,
            compression: SelectCompression::None,
        };
        let json_output = build_output_serialization(&json_options);
        assert!(json_output.json().is_some());
        assert!(json_output.csv().is_none());
    }
}
