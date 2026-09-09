//! S3 Select domain types (no AWS SDK types).

use thiserror::Error;

/// Object payload format for S3 Select input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectInputFormat {
    #[default]
    Csv,
    Json,
    Parquet,
}

/// Result row format for S3 Select output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectOutputFormat {
    #[default]
    Csv,
    Json,
}

/// Compression applied to the **stored object** (input decompression).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectCompression {
    #[default]
    None,
    Gzip,
    Bzip2,
}

/// CSV header handling for S3 Select input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectCsvFileHeaderInfo {
    #[default]
    None,
    Ignore,
    Use,
}

/// JSON input shape for S3 Select.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectJsonInputType {
    #[default]
    Lines,
    Document,
}

/// CSV output quote behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectQuoteFields {
    Always,
    #[default]
    AsNeeded,
}

/// Supported CSV input serialization options.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SelectCsvInputOptions {
    pub file_header_info: SelectCsvFileHeaderInfo,
    pub field_delimiter: Option<String>,
    pub record_delimiter: Option<String>,
    pub quote_character: Option<String>,
    pub quote_escape_character: Option<String>,
    pub comments: Option<String>,
}

/// Supported CSV output serialization options.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SelectCsvOutputOptions {
    pub field_delimiter: Option<String>,
    pub record_delimiter: Option<String>,
    pub quote_character: Option<String>,
    pub quote_escape_character: Option<String>,
    pub quote_fields: SelectQuoteFields,
}

/// Supported JSON input serialization options.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SelectJsonInputOptions {
    pub input_type: SelectJsonInputType,
}

/// Supported JSON output serialization options.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SelectJsonOutputOptions {
    pub record_delimiter: Option<String>,
}

/// ScanRange request body parameters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SelectScanRangeOptions {
    pub start: Option<i64>,
    pub end: Option<i64>,
}

/// SSE-C parameters for encrypted objects.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SelectSseCustomerOptions {
    pub algorithm: Option<String>,
    pub key: Option<String>,
    pub key_md5: Option<String>,
}

/// Options for running an S3 Select query on one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOptions {
    /// SQL expression (S3 Select / `s3object`).
    pub expression: String,
    pub input_format: SelectInputFormat,
    pub output_format: SelectOutputFormat,
    pub compression: SelectCompression,
    pub csv_input: SelectCsvInputOptions,
    pub csv_output: SelectCsvOutputOptions,
    pub json_input: SelectJsonInputOptions,
    pub json_output: SelectJsonOutputOptions,
    pub scan_range: SelectScanRangeOptions,
    pub sse_customer: SelectSseCustomerOptions,
}

/// Invalid combinations or values in an S3 Select request.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SelectOptionsError {
    #[error("{field} must be exactly one byte")]
    InvalidSingleByte { field: &'static str },
    #[error("CSV input record delimiter must be one or two bytes")]
    InvalidCsvInputRecordDelimiter,
    #[error("CSV output record delimiter must be exactly one byte or CRLF")]
    InvalidCsvOutputRecordDelimiter,
    #[error("Parquet input does not support whole-object GZIP or BZIP2 compression")]
    CompressedParquetInput,
    #[error("ScanRange is not supported for JSON document input")]
    JsonDocumentScanRange,
    #[error("ScanRange is not supported for compressed input")]
    CompressedInputScanRange,
    #[error("ScanRange start and end must be non-negative")]
    NegativeScanRange,
    #[error("ScanRange start must not be greater than end")]
    ReversedScanRange,
}

impl SelectOptions {
    /// Validate format-specific options before a request reaches an object store.
    pub fn validate(&self) -> std::result::Result<(), SelectOptionsError> {
        if matches!(self.input_format, SelectInputFormat::Csv) {
            validate_single_byte(
                "CSV input field delimiter",
                self.csv_input.field_delimiter.as_deref(),
            )?;
            validate_input_record_delimiter(self.csv_input.record_delimiter.as_deref())?;
            validate_single_byte(
                "CSV input quote character",
                self.csv_input.quote_character.as_deref(),
            )?;
            validate_single_byte(
                "CSV input quote escape character",
                self.csv_input.quote_escape_character.as_deref(),
            )?;
            validate_single_byte(
                "CSV input comment character",
                self.csv_input.comments.as_deref(),
            )?;
        }

        if matches!(self.output_format, SelectOutputFormat::Csv) {
            validate_single_byte(
                "CSV output field delimiter",
                self.csv_output.field_delimiter.as_deref(),
            )?;
            validate_output_record_delimiter(self.csv_output.record_delimiter.as_deref())?;
            validate_single_byte(
                "CSV output quote character",
                self.csv_output.quote_character.as_deref(),
            )?;
            validate_single_byte(
                "CSV output quote escape character",
                self.csv_output.quote_escape_character.as_deref(),
            )?;
        }

        if matches!(self.input_format, SelectInputFormat::Parquet)
            && !matches!(self.compression, SelectCompression::None)
        {
            return Err(SelectOptionsError::CompressedParquetInput);
        }

        self.validate_scan_range()
    }

    fn validate_scan_range(&self) -> std::result::Result<(), SelectOptionsError> {
        let scan_range = &self.scan_range;
        if scan_range.start.is_none() && scan_range.end.is_none() {
            return Ok(());
        }
        if matches!(self.input_format, SelectInputFormat::Json)
            && matches!(self.json_input.input_type, SelectJsonInputType::Document)
        {
            return Err(SelectOptionsError::JsonDocumentScanRange);
        }
        let is_noop = scan_range.start == Some(0) && scan_range.end.is_none();
        if !matches!(self.compression, SelectCompression::None) && !is_noop {
            return Err(SelectOptionsError::CompressedInputScanRange);
        }
        if scan_range.start.is_some_and(|start| start < 0)
            || scan_range.end.is_some_and(|end| end < 0)
        {
            return Err(SelectOptionsError::NegativeScanRange);
        }
        if let (Some(start), Some(end)) = (scan_range.start, scan_range.end)
            && start > end
        {
            return Err(SelectOptionsError::ReversedScanRange);
        }
        Ok(())
    }
}

fn validate_single_byte(
    field: &'static str,
    value: Option<&str>,
) -> std::result::Result<(), SelectOptionsError> {
    if value.is_some_and(|value| value.len() != 1) {
        return Err(SelectOptionsError::InvalidSingleByte { field });
    }
    Ok(())
}

fn validate_input_record_delimiter(
    value: Option<&str>,
) -> std::result::Result<(), SelectOptionsError> {
    if value.is_some_and(|value| !(1..=2).contains(&value.len())) {
        return Err(SelectOptionsError::InvalidCsvInputRecordDelimiter);
    }
    Ok(())
}

fn validate_output_record_delimiter(
    value: Option<&str>,
) -> std::result::Result<(), SelectOptionsError> {
    if value.is_some_and(|value| value.len() != 1 && value != "\r\n") {
        return Err(SelectOptionsError::InvalidCsvOutputRecordDelimiter);
    }
    Ok(())
}

impl Default for SelectOptions {
    fn default() -> Self {
        Self {
            expression: String::new(),
            input_format: SelectInputFormat::Csv,
            output_format: SelectOutputFormat::Csv,
            compression: SelectCompression::None,
            csv_input: SelectCsvInputOptions::default(),
            csv_output: SelectCsvOutputOptions::default(),
            json_input: SelectJsonInputOptions::default(),
            json_output: SelectJsonOutputOptions::default(),
            scan_range: SelectScanRangeOptions::default(),
            sse_customer: SelectSseCustomerOptions::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_allows_parquet_scan_range() {
        let options = SelectOptions {
            input_format: SelectInputFormat::Parquet,
            scan_range: SelectScanRangeOptions {
                start: Some(1024),
                end: Some(2047),
            },
            ..SelectOptions::default()
        };

        options
            .validate()
            .expect("Parquet scan range should be supported");
    }

    #[test]
    fn validation_rejects_non_noop_scan_range_for_compressed_input() {
        let options = SelectOptions {
            compression: SelectCompression::Gzip,
            scan_range: SelectScanRangeOptions {
                start: Some(1),
                end: None,
            },
            ..SelectOptions::default()
        };

        assert_eq!(
            options.validate(),
            Err(SelectOptionsError::CompressedInputScanRange)
        );
    }

    #[test]
    fn validation_allows_two_byte_csv_input_record_delimiter() {
        let options = SelectOptions {
            csv_input: SelectCsvInputOptions {
                record_delimiter: Some("\r\n".to_string()),
                ..SelectCsvInputOptions::default()
            },
            ..SelectOptions::default()
        };

        options
            .validate()
            .expect("two-byte CSV input record delimiter should be supported");
    }

    #[test]
    fn validation_rejects_empty_csv_input_record_delimiter() {
        let options = SelectOptions {
            csv_input: SelectCsvInputOptions {
                record_delimiter: Some(String::new()),
                ..SelectCsvInputOptions::default()
            },
            ..SelectOptions::default()
        };

        assert_eq!(
            options.validate(),
            Err(SelectOptionsError::InvalidCsvInputRecordDelimiter)
        );
    }

    #[test]
    fn validation_ignores_csv_options_for_non_csv_formats() {
        let options = SelectOptions {
            input_format: SelectInputFormat::Json,
            output_format: SelectOutputFormat::Json,
            csv_input: SelectCsvInputOptions {
                record_delimiter: Some(String::new()),
                ..SelectCsvInputOptions::default()
            },
            csv_output: SelectCsvOutputOptions {
                field_delimiter: Some("||".to_string()),
                ..SelectCsvOutputOptions::default()
            },
            ..SelectOptions::default()
        };

        options
            .validate()
            .expect("inactive CSV options should not affect JSON requests");
    }
}
