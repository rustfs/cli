//! sql command — S3 Select queries on objects.

use clap::{Args, ValueEnum};
use rc_core::{
    AliasManager, ObjectStore, SelectCompression, SelectCsvFileHeaderInfo, SelectCsvInputOptions,
    SelectCsvOutputOptions, SelectInputFormat, SelectJsonInputOptions, SelectJsonInputType,
    SelectJsonOutputOptions, SelectOptions, SelectOutputFormat, SelectQuoteFields,
    SelectScanRangeOptions, SelectSseCustomerOptions, parse_object_path,
};
use rc_s3::S3Client;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// Run an S3 Select SQL query on an object and stream results to stdout.
#[derive(Args, Debug)]
pub struct SqlArgs {
    /// Object path (alias/bucket/key)
    pub path: String,

    /// SQL expression (S3 Select)
    #[arg(long)]
    pub query: String,

    /// Input object format
    #[arg(long, value_enum, default_value_t = InputFormatArg::Csv)]
    pub input_format: InputFormatArg,

    /// Select result format
    #[arg(long, value_enum, default_value_t = OutputFormatArg::Csv)]
    pub output_format: OutputFormatArg,

    /// Compression of the stored object (input decompression)
    #[arg(long, value_enum, default_value_t = CompressionArg::None)]
    pub compression: CompressionArg,

    /// CSV input header handling
    #[arg(long, value_enum, default_value_t = CsvFileHeaderInfoArg::None)]
    pub csv_file_header_info: CsvFileHeaderInfoArg,

    /// CSV input field delimiter
    #[arg(long)]
    pub csv_input_field_delimiter: Option<String>,

    /// CSV input quote character
    #[arg(long)]
    pub csv_input_quote: Option<String>,

    /// CSV input quote escape character
    #[arg(long)]
    pub csv_input_quote_escape: Option<String>,

    /// CSV input comment character
    #[arg(long)]
    pub csv_input_comment: Option<String>,

    /// CSV output field delimiter
    #[arg(long)]
    pub csv_output_field_delimiter: Option<String>,

    /// CSV output record delimiter
    #[arg(long)]
    pub csv_output_record_delimiter: Option<String>,

    /// CSV output quote character
    #[arg(long)]
    pub csv_output_quote: Option<String>,

    /// CSV output quote escape character
    #[arg(long)]
    pub csv_output_quote_escape: Option<String>,

    /// CSV output quote behavior
    #[arg(long, value_enum, default_value_t = QuoteFieldsArg::AsNeeded)]
    pub csv_output_quote_fields: QuoteFieldsArg,

    /// JSON input shape
    #[arg(long, value_enum, default_value_t = JsonTypeArg::Lines)]
    pub json_type: JsonTypeArg,

    /// JSON output record delimiter
    #[arg(long)]
    pub json_output_record_delimiter: Option<String>,

    /// Select ScanRange start byte
    #[arg(long)]
    pub scan_start: Option<i64>,

    /// Select ScanRange end byte
    #[arg(long)]
    pub scan_end: Option<i64>,

    /// SSE-C customer algorithm, usually AES256
    #[arg(long)]
    pub sse_customer_algorithm: Option<String>,

    /// SSE-C customer key
    #[arg(long)]
    pub sse_customer_key: Option<String>,

    /// SSE-C customer key MD5
    #[arg(long)]
    pub sse_customer_key_md5: Option<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum InputFormatArg {
    Csv,
    /// Newline-delimited JSON (S3 Select `Type=LINES`).
    Json,
    Parquet,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum OutputFormatArg {
    Csv,
    Json,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum CompressionArg {
    None,
    Gzip,
    Bzip2,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum CsvFileHeaderInfoArg {
    None,
    Ignore,
    Use,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum JsonTypeArg {
    Lines,
    Document,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum QuoteFieldsArg {
    Always,
    AsNeeded,
}

impl From<InputFormatArg> for SelectInputFormat {
    fn from(value: InputFormatArg) -> Self {
        match value {
            InputFormatArg::Csv => SelectInputFormat::Csv,
            InputFormatArg::Json => SelectInputFormat::Json,
            InputFormatArg::Parquet => SelectInputFormat::Parquet,
        }
    }
}

impl From<OutputFormatArg> for SelectOutputFormat {
    fn from(value: OutputFormatArg) -> Self {
        match value {
            OutputFormatArg::Csv => SelectOutputFormat::Csv,
            OutputFormatArg::Json => SelectOutputFormat::Json,
        }
    }
}

impl From<CompressionArg> for SelectCompression {
    fn from(value: CompressionArg) -> Self {
        match value {
            CompressionArg::None => SelectCompression::None,
            CompressionArg::Gzip => SelectCompression::Gzip,
            CompressionArg::Bzip2 => SelectCompression::Bzip2,
        }
    }
}

impl From<CsvFileHeaderInfoArg> for SelectCsvFileHeaderInfo {
    fn from(value: CsvFileHeaderInfoArg) -> Self {
        match value {
            CsvFileHeaderInfoArg::None => SelectCsvFileHeaderInfo::None,
            CsvFileHeaderInfoArg::Ignore => SelectCsvFileHeaderInfo::Ignore,
            CsvFileHeaderInfoArg::Use => SelectCsvFileHeaderInfo::Use,
        }
    }
}

impl From<JsonTypeArg> for SelectJsonInputType {
    fn from(value: JsonTypeArg) -> Self {
        match value {
            JsonTypeArg::Lines => SelectJsonInputType::Lines,
            JsonTypeArg::Document => SelectJsonInputType::Document,
        }
    }
}

impl From<QuoteFieldsArg> for SelectQuoteFields {
    fn from(value: QuoteFieldsArg) -> Self {
        match value {
            QuoteFieldsArg::Always => SelectQuoteFields::Always,
            QuoteFieldsArg::AsNeeded => SelectQuoteFields::AsNeeded,
        }
    }
}

/// Execute the `sql` command.
pub async fn execute(args: SqlArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    if args.query.trim().is_empty() {
        formatter.error("Query must not be empty (--query)");
        return ExitCode::UsageError;
    }

    let remote = match parse_object_path(&args.path) {
        Ok(p) => p,
        Err(e) => {
            formatter.error(&e.to_string());
            return ExitCode::UsageError;
        }
    };

    if let Err(message) = validate_select_args(&args) {
        formatter.error(&message);
        return ExitCode::UsageError;
    }

    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return ExitCode::GeneralError;
        }
    };

    let alias = match alias_manager.get(&remote.alias) {
        Ok(a) => a,
        Err(_) => {
            formatter.error(&format!("Alias '{}' not found", remote.alias));
            return ExitCode::NotFound;
        }
    };

    let client = match S3Client::new(alias).await {
        Ok(c) => c,
        Err(e) => {
            formatter.error(&format!("Failed to create S3 client: {e}"));
            return ExitCode::NetworkError;
        }
    };

    let options = SelectOptions {
        expression: args.query,
        input_format: args.input_format.into(),
        output_format: args.output_format.into(),
        compression: args.compression.into(),
        csv_input: SelectCsvInputOptions {
            file_header_info: args.csv_file_header_info.into(),
            field_delimiter: args.csv_input_field_delimiter,
            quote_character: args.csv_input_quote,
            quote_escape_character: args.csv_input_quote_escape,
            comments: args.csv_input_comment,
        },
        csv_output: SelectCsvOutputOptions {
            field_delimiter: args.csv_output_field_delimiter,
            record_delimiter: args.csv_output_record_delimiter,
            quote_character: args.csv_output_quote,
            quote_escape_character: args.csv_output_quote_escape,
            quote_fields: args.csv_output_quote_fields.into(),
        },
        json_input: SelectJsonInputOptions {
            input_type: args.json_type.into(),
        },
        json_output: SelectJsonOutputOptions {
            record_delimiter: args.json_output_record_delimiter,
        },
        scan_range: SelectScanRangeOptions {
            start: args.scan_start,
            end: args.scan_end,
        },
        sse_customer: SelectSseCustomerOptions {
            algorithm: args.sse_customer_algorithm,
            key: args.sse_customer_key,
            key_md5: args.sse_customer_key_md5,
        },
    };

    let mut stdout = tokio::io::stdout();

    match client
        .select_object_content(&remote, &options, &mut stdout)
        .await
    {
        Ok(()) => ExitCode::Success,
        Err(e) => {
            formatter.error(&e.to_string());
            exit_code_from_error(&e)
        }
    }
}

fn validate_select_args(args: &SqlArgs) -> std::result::Result<(), String> {
    validate_single_byte(
        "--csv-input-field-delimiter",
        args.csv_input_field_delimiter.as_deref(),
    )?;
    validate_single_byte("--csv-input-quote", args.csv_input_quote.as_deref())?;
    validate_single_byte(
        "--csv-input-quote-escape",
        args.csv_input_quote_escape.as_deref(),
    )?;
    validate_single_byte("--csv-input-comment", args.csv_input_comment.as_deref())?;
    validate_single_byte(
        "--csv-output-field-delimiter",
        args.csv_output_field_delimiter.as_deref(),
    )?;
    validate_record_delimiter(
        "--csv-output-record-delimiter",
        args.csv_output_record_delimiter.as_deref(),
    )?;
    validate_single_byte("--csv-output-quote", args.csv_output_quote.as_deref())?;
    validate_single_byte(
        "--csv-output-quote-escape",
        args.csv_output_quote_escape.as_deref(),
    )?;
    validate_scan_range_args(args)
}

fn validate_single_byte(name: &str, value: Option<&str>) -> std::result::Result<(), String> {
    if let Some(value) = value
        && value.len() != 1
    {
        return Err(format!("{name} must be exactly one byte"));
    }
    Ok(())
}

fn validate_record_delimiter(name: &str, value: Option<&str>) -> std::result::Result<(), String> {
    if let Some(value) = value
        && value.len() != 1
        && value != "\r\n"
    {
        return Err(format!("{name} must be exactly one byte or CRLF"));
    }
    Ok(())
}

fn validate_scan_range_args(args: &SqlArgs) -> std::result::Result<(), String> {
    if args.scan_start.is_none() && args.scan_end.is_none() {
        return Ok(());
    }
    if matches!(args.input_format, InputFormatArg::Parquet) {
        return Err("ScanRange is not supported for Parquet input".to_string());
    }
    if matches!(args.input_format, InputFormatArg::Json)
        && matches!(args.json_type, JsonTypeArg::Document)
    {
        return Err("ScanRange is not supported for JSON document input".to_string());
    }
    if args.scan_start.is_some_and(|start| start < 0) || args.scan_end.is_some_and(|end| end < 0) {
        return Err("ScanRange start and end must be non-negative".to_string());
    }
    if let (Some(start), Some(end)) = (args.scan_start, args.scan_end)
        && start > end
    {
        return Err("ScanRange start must not be greater than end".to_string());
    }
    Ok(())
}

fn exit_code_from_error(error: &rc_core::Error) -> ExitCode {
    ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputConfig;
    use rc_core::Error;

    fn base_args(path: &str, query: &str) -> SqlArgs {
        SqlArgs {
            path: path.to_string(),
            query: query.to_string(),
            input_format: InputFormatArg::Csv,
            output_format: OutputFormatArg::Csv,
            compression: CompressionArg::None,
            csv_file_header_info: CsvFileHeaderInfoArg::None,
            csv_input_field_delimiter: None,
            csv_input_quote: None,
            csv_input_quote_escape: None,
            csv_input_comment: None,
            csv_output_field_delimiter: None,
            csv_output_record_delimiter: None,
            csv_output_quote: None,
            csv_output_quote_escape: None,
            csv_output_quote_fields: QuoteFieldsArg::AsNeeded,
            json_type: JsonTypeArg::Lines,
            json_output_record_delimiter: None,
            scan_start: None,
            scan_end: None,
            sse_customer_algorithm: None,
            sse_customer_key: None,
            sse_customer_key_md5: None,
        }
    }

    #[tokio::test]
    async fn sql_empty_query_is_usage_error() {
        let args = base_args("a/b/c", "  ");
        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn sql_invalid_object_path_is_usage_error() {
        let args = base_args("a/b", "SELECT 1");
        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn sql_rejects_multi_byte_csv_delimiter() {
        let mut args = base_args("a/b/c", "SELECT * FROM S3Object");
        args.csv_input_field_delimiter = Some("||".to_string());
        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn sql_rejects_scan_range_for_json_document() {
        let mut args = base_args("a/b/c", "SELECT * FROM S3Object");
        args.input_format = InputFormatArg::Json;
        args.json_type = JsonTypeArg::Document;
        args.scan_start = Some(0);
        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn sql_rejects_scan_start_after_end() {
        let mut args = base_args("a/b/c", "SELECT * FROM S3Object");
        args.scan_start = Some(20);
        args.scan_end = Some(10);
        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[test]
    fn sql_exit_code_from_backend_errors() {
        let cases = [
            (
                Error::UnsupportedFeature("S3 Select is not supported".to_string()),
                ExitCode::UnsupportedFeature,
            ),
            (
                Error::NotFound("Object not found".to_string()),
                ExitCode::NotFound,
            ),
            (
                Error::Auth("Access denied".to_string()),
                ExitCode::AuthError,
            ),
            (
                Error::Network("Request timeout".to_string()),
                ExitCode::NetworkError,
            ),
            (
                Error::General("Query failed".to_string()),
                ExitCode::GeneralError,
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(exit_code_from_error(&error), expected);
        }
    }
}
