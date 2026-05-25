//! S3 Select (`SelectObjectContent`) — AWS SDK mapping and streaming.

use aws_sdk_s3::types::{
    CompressionType, CsvInput, CsvOutput, ExpressionType, FileHeaderInfo, InputSerialization,
    JsonInput, JsonOutput, JsonType, OutputSerialization, ParquetInput, QuoteFields, ScanRange,
    SelectObjectContentEventStream,
};
use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
use aws_smithy_runtime_api::client::result::SdkError;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use aws_smithy_types::event_stream::RawMessage;
use rc_core::{
    Error, RemotePath, Result, SelectCompression, SelectCsvFileHeaderInfo, SelectInputFormat,
    SelectJsonInputType, SelectOptions, SelectOutputFormat,
    SelectQuoteFields as RcSelectQuoteFields,
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
    let output = build_output_serialization(options)?;

    // aws-sdk-s3 `SelectObjectContent` does not expose object `VersionId`; the current object is used.
    let mut request = client
        .select_object_content()
        .bucket(&path.bucket)
        .key(&path.key)
        .expression(&options.expression)
        .expression_type(ExpressionType::Sql)
        .input_serialization(input)
        .output_serialization(output);
    if let Some(scan_range) = build_scan_range(options)? {
        request = request.scan_range(scan_range);
    }
    if let Some(algorithm) = options.sse_customer.algorithm.as_deref() {
        request = request.sse_customer_algorithm(algorithm);
    }
    if let Some(key) = options.sse_customer.key.as_deref() {
        request = request.sse_customer_key(key);
    }
    if let Some(key_md5) = options.sse_customer.key_md5.as_deref() {
        request = request.sse_customer_key_md5(key_md5);
    }

    let resp = request.send().await.map_err(map_select_initial_error)?;

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

fn csv_file_header_info(info: SelectCsvFileHeaderInfo) -> FileHeaderInfo {
    match info {
        SelectCsvFileHeaderInfo::None => FileHeaderInfo::None,
        SelectCsvFileHeaderInfo::Ignore => FileHeaderInfo::Ignore,
        SelectCsvFileHeaderInfo::Use => FileHeaderInfo::Use,
    }
}

fn json_input_type(input_type: SelectJsonInputType) -> JsonType {
    match input_type {
        SelectJsonInputType::Lines => JsonType::Lines,
        SelectJsonInputType::Document => JsonType::Document,
    }
}

fn quote_fields(quote_fields: RcSelectQuoteFields) -> QuoteFields {
    match quote_fields {
        RcSelectQuoteFields::Always => QuoteFields::Always,
        RcSelectQuoteFields::AsNeeded => QuoteFields::Asneeded,
    }
}

fn build_scan_range(options: &SelectOptions) -> Result<Option<ScanRange>> {
    let scan_range = &options.scan_range;
    if scan_range.start.is_none() && scan_range.end.is_none() {
        return Ok(None);
    }
    if matches!(options.input_format, SelectInputFormat::Parquet) {
        return Err(Error::General(
            "ScanRange is not supported for Parquet input.".to_string(),
        ));
    }
    if matches!(options.input_format, SelectInputFormat::Json)
        && matches!(options.json_input.input_type, SelectJsonInputType::Document)
    {
        return Err(Error::General(
            "ScanRange is not supported for JSON document input.".to_string(),
        ));
    }
    if scan_range.start.is_some_and(|start| start < 0) || scan_range.end.is_some_and(|end| end < 0)
    {
        return Err(Error::General(
            "ScanRange start and end must be non-negative.".to_string(),
        ));
    }
    if let (Some(start), Some(end)) = (scan_range.start, scan_range.end)
        && start > end
    {
        return Err(Error::General(
            "ScanRange start must not be greater than end.".to_string(),
        ));
    }
    Ok(Some(
        ScanRange::builder()
            .set_start(scan_range.start)
            .set_end(scan_range.end)
            .build(),
    ))
}

fn validate_single_byte(name: &str, value: Option<&str>) -> Result<()> {
    if let Some(value) = value
        && value.len() != 1
    {
        return Err(Error::General(format!("{name} must be exactly one byte.")));
    }
    Ok(())
}

fn validate_record_delimiter(name: &str, value: Option<&str>) -> Result<()> {
    if let Some(value) = value
        && value.len() != 1
        && value != "\r\n"
    {
        return Err(Error::General(format!(
            "{name} must be exactly one byte or CRLF."
        )));
    }
    Ok(())
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
            validate_single_byte(
                "CSV input field delimiter",
                options.csv_input.field_delimiter.as_deref(),
            )?;
            validate_single_byte(
                "CSV input quote character",
                options.csv_input.quote_character.as_deref(),
            )?;
            validate_single_byte(
                "CSV input quote escape character",
                options.csv_input.quote_escape_character.as_deref(),
            )?;
            validate_single_byte(
                "CSV input comment character",
                options.csv_input.comments.as_deref(),
            )?;
            let mut csv = CsvInput::builder()
                .file_header_info(csv_file_header_info(options.csv_input.file_header_info));
            if let Some(delimiter) = options.csv_input.field_delimiter.as_deref() {
                csv = csv.field_delimiter(delimiter);
            }
            if let Some(quote) = options.csv_input.quote_character.as_deref() {
                csv = csv.quote_character(quote);
            }
            if let Some(escape) = options.csv_input.quote_escape_character.as_deref() {
                csv = csv.quote_escape_character(escape);
            }
            if let Some(comments) = options.csv_input.comments.as_deref() {
                csv = csv.comments(comments);
            }
            let csv = csv.build();
            b = b.csv(csv);
        }
        SelectInputFormat::Json => {
            let json = JsonInput::builder()
                .r#type(json_input_type(options.json_input.input_type))
                .build();
            b = b.json(json);
        }
        SelectInputFormat::Parquet => {
            let pq = ParquetInput::builder().build();
            b = b.parquet(pq);
        }
    }
    Ok(b.build())
}

fn build_output_serialization(options: &SelectOptions) -> Result<OutputSerialization> {
    let mut b = OutputSerialization::builder();
    match options.output_format {
        SelectOutputFormat::Csv => {
            validate_single_byte(
                "CSV output field delimiter",
                options.csv_output.field_delimiter.as_deref(),
            )?;
            validate_record_delimiter(
                "CSV output record delimiter",
                options.csv_output.record_delimiter.as_deref(),
            )?;
            validate_single_byte(
                "CSV output quote character",
                options.csv_output.quote_character.as_deref(),
            )?;
            validate_single_byte(
                "CSV output quote escape character",
                options.csv_output.quote_escape_character.as_deref(),
            )?;
            let mut csv =
                CsvOutput::builder().quote_fields(quote_fields(options.csv_output.quote_fields));
            if let Some(delimiter) = options.csv_output.field_delimiter.as_deref() {
                csv = csv.field_delimiter(delimiter);
            }
            if let Some(record_delimiter) = options.csv_output.record_delimiter.as_deref() {
                csv = csv.record_delimiter(record_delimiter);
            }
            if let Some(quote) = options.csv_output.quote_character.as_deref() {
                csv = csv.quote_character(quote);
            }
            if let Some(escape) = options.csv_output.quote_escape_character.as_deref() {
                csv = csv.quote_escape_character(escape);
            }
            let csv = csv.build();
            b = b.csv(csv);
        }
        SelectOutputFormat::Json => {
            let mut json = JsonOutput::builder();
            if let Some(record_delimiter) = options.json_output.record_delimiter.as_deref() {
                json = json.record_delimiter(record_delimiter);
            }
            let json = json.build();
            b = b.json(json);
        }
    }
    Ok(b.build())
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
    use super::{
        build_input_serialization, build_output_serialization, build_scan_range, classify_aws_code,
    };
    use aws_sdk_s3::types::{CompressionType, FileHeaderInfo, JsonType, QuoteFields};
    use rc_core::Error;
    use rc_core::{
        SelectCompression, SelectCsvInputOptions, SelectCsvOutputOptions, SelectInputFormat,
        SelectJsonInputOptions, SelectJsonInputType, SelectOptions, SelectOutputFormat,
        SelectScanRangeOptions,
    };

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
    fn classify_empty_code_maps_no_such_key_substring() {
        let e = classify_aws_code(Some(""), "Service error: ... NoSuchKey ...");
        assert!(matches!(e, Error::NotFound(msg) if msg.contains("Object")));
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
            ..SelectOptions::default()
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
            ..SelectOptions::default()
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
            ..SelectOptions::default()
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
            ..SelectOptions::default()
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
            ..SelectOptions::default()
        };
        let csv_output = build_output_serialization(&csv_options).expect("csv output");
        let csv = csv_output.csv().expect("csv output is configured");
        assert_eq!(csv.quote_fields(), Some(&QuoteFields::Asneeded));
        assert!(csv_output.json().is_none());

        let json_options = SelectOptions {
            expression: "SELECT * FROM S3Object".to_string(),
            input_format: SelectInputFormat::Json,
            output_format: SelectOutputFormat::Json,
            compression: SelectCompression::None,
            ..SelectOptions::default()
        };
        let json_output = build_output_serialization(&json_options).expect("json output");
        assert!(json_output.json().is_some());
        assert!(json_output.csv().is_none());
    }

    #[test]
    fn csv_input_rejects_multi_byte_delimiter() {
        let options = SelectOptions {
            expression: "SELECT * FROM S3Object".to_string(),
            csv_input: SelectCsvInputOptions {
                field_delimiter: Some("||".to_string()),
                ..SelectCsvInputOptions::default()
            },
            ..SelectOptions::default()
        };

        let error = build_input_serialization(&options)
            .expect_err("multi-byte CSV input delimiter should be rejected");
        assert!(matches!(error, Error::General(msg) if msg.contains("field delimiter")));
    }

    #[test]
    fn csv_output_record_delimiter_allows_crlf() {
        let options = SelectOptions {
            expression: "SELECT * FROM S3Object".to_string(),
            csv_output: SelectCsvOutputOptions {
                record_delimiter: Some("\r\n".to_string()),
                ..SelectCsvOutputOptions::default()
            },
            ..SelectOptions::default()
        };

        let output = build_output_serialization(&options).expect("CRLF record delimiter");
        let csv = output.csv().expect("csv output is configured");
        assert_eq!(csv.record_delimiter(), Some("\r\n"));
    }

    #[test]
    fn csv_output_rejects_multi_byte_record_delimiter() {
        let options = SelectOptions {
            expression: "SELECT * FROM S3Object".to_string(),
            csv_output: SelectCsvOutputOptions {
                record_delimiter: Some("||".to_string()),
                ..SelectCsvOutputOptions::default()
            },
            ..SelectOptions::default()
        };

        let error = build_output_serialization(&options)
            .expect_err("multi-byte CSV output record delimiter should be rejected");
        assert!(matches!(error, Error::General(msg) if msg.contains("record delimiter")));
    }

    #[test]
    fn scan_range_rejects_json_document() {
        let options = SelectOptions {
            expression: "SELECT * FROM S3Object".to_string(),
            input_format: SelectInputFormat::Json,
            json_input: SelectJsonInputOptions {
                input_type: SelectJsonInputType::Document,
            },
            scan_range: SelectScanRangeOptions {
                start: Some(0),
                end: None,
            },
            ..SelectOptions::default()
        };

        let error =
            build_scan_range(&options).expect_err("scan range should reject JSON document input");
        assert!(matches!(error, Error::General(msg) if msg.contains("JSON document")));
    }

    #[test]
    fn scan_range_rejects_start_after_end() {
        let options = SelectOptions {
            expression: "SELECT * FROM S3Object".to_string(),
            scan_range: SelectScanRangeOptions {
                start: Some(20),
                end: Some(10),
            },
            ..SelectOptions::default()
        };

        let error = build_scan_range(&options).expect_err("start after end should be rejected");
        assert!(matches!(error, Error::General(msg) if msg.contains("greater than end")));
    }
}
