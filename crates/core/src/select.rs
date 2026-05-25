//! S3 Select domain types (no AWS SDK types).

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
