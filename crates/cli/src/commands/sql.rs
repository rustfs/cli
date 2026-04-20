//! sql command — S3 Select queries on objects.

use clap::{Args, ValueEnum};
use rc_core::{
    AliasManager, ObjectStore, SelectCompression, SelectInputFormat, SelectOptions,
    SelectOutputFormat, parse_object_path,
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

fn exit_code_from_error(error: &rc_core::Error) -> ExitCode {
    ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputConfig;

    #[tokio::test]
    async fn sql_empty_query_is_usage_error() {
        let args = SqlArgs {
            path: "a/b/c".to_string(),
            query: "  ".to_string(),
            input_format: InputFormatArg::Csv,
            output_format: OutputFormatArg::Csv,
            compression: CompressionArg::None,
        };
        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn sql_invalid_object_path_is_usage_error() {
        let args = SqlArgs {
            path: "a/b".to_string(),
            query: "SELECT 1".to_string(),
            input_format: InputFormatArg::Csv,
            output_format: OutputFormatArg::Csv,
            compression: CompressionArg::None,
        };
        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }
}
