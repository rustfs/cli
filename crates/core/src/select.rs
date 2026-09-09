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

/// Invalid combinations or values for an S3 Select scan range.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SelectScanRangeError {
    #[error("ScanRange is not supported for JSON document input")]
    JsonDocument,
    #[error("ScanRange is not supported for compressed input")]
    CompressedInput,
    #[error("ScanRange start and end must be non-negative")]
    NegativeBounds,
    #[error("ScanRange start must not be greater than end")]
    ReversedBounds,
}

impl SelectScanRangeOptions {
    /// Validate this range against the selected input serialization.
    pub fn validate_for_input(
        &self,
        input_format: SelectInputFormat,
        json_input_type: SelectJsonInputType,
        compression: SelectCompression,
    ) -> std::result::Result<(), SelectScanRangeError> {
        if self.start.is_none() && self.end.is_none() {
            return Ok(());
        }
        if matches!(input_format, SelectInputFormat::Json)
            && matches!(json_input_type, SelectJsonInputType::Document)
        {
            return Err(SelectScanRangeError::JsonDocument);
        }
        let is_noop = self.start == Some(0) && self.end.is_none();
        if !matches!(compression, SelectCompression::None) && !is_noop {
            return Err(SelectScanRangeError::CompressedInput);
        }
        if self.start.is_some_and(|start| start < 0) || self.end.is_some_and(|end| end < 0) {
            return Err(SelectScanRangeError::NegativeBounds);
        }
        if let (Some(start), Some(end)) = (self.start, self.end)
            && start > end
        {
            return Err(SelectScanRangeError::ReversedBounds);
        }
        Ok(())
    }
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
    fn scan_range_allows_parquet_input() {
        let range = SelectScanRangeOptions {
            start: Some(1024),
            end: Some(2047),
        };

        range
            .validate_for_input(
                SelectInputFormat::Parquet,
                SelectJsonInputType::Lines,
                SelectCompression::None,
            )
            .expect("Parquet scan ranges should be supported");
    }

    #[test]
    fn scan_range_rejects_compressed_input() {
        let range = SelectScanRangeOptions {
            start: Some(1),
            end: None,
        };

        assert_eq!(
            range.validate_for_input(
                SelectInputFormat::Csv,
                SelectJsonInputType::Lines,
                SelectCompression::Gzip,
            ),
            Err(SelectScanRangeError::CompressedInput)
        );
    }

    #[test]
    fn scan_range_allows_noop_for_compressed_input() {
        let range = SelectScanRangeOptions {
            start: Some(0),
            end: None,
        };

        range
            .validate_for_input(
                SelectInputFormat::Csv,
                SelectJsonInputType::Lines,
                SelectCompression::Bzip2,
            )
            .expect("RustFS accepts a no-op scan range for compressed input");
    }
}
