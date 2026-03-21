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

/// Options for running an S3 Select query on one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOptions {
    /// SQL expression (S3 Select / `s3object`).
    pub expression: String,
    pub input_format: SelectInputFormat,
    pub output_format: SelectOutputFormat,
    pub compression: SelectCompression,
}

impl Default for SelectOptions {
    fn default() -> Self {
        Self {
            expression: String::new(),
            input_format: SelectInputFormat::Csv,
            output_format: SelectOutputFormat::Csv,
            compression: SelectCompression::None,
        }
    }
}
