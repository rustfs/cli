//! Bounded active client-devnull diagnostic.

use std::future::Future;
use std::time::Duration;

use clap::Args;
use rc_core::Error;
use rc_core::admin::{
    ClientDevnullRequest, ClientDevnullResult, DEFAULT_CLIENT_DEVNULL_CONCURRENCY, DiagnosticApi,
};
use serde::Serialize;

use super::super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;

const CLIENT_DEVNULL_CAPABILITY: &str = "admin.diagnostics.client-devnull";

/// Options for the bounded client-to-server devnull probe.
#[derive(Args, Debug, Clone)]
pub struct ClientDevnullArgs {
    /// Alias name of the RustFS server
    pub alias: String,

    /// Bytes uploaded by each request (for example 8MiB)
    #[arg(long, default_value = "8MiB")]
    pub size: String,

    /// Active probe timeout in whole seconds (for example 30s)
    #[arg(long, default_value = "30s")]
    pub timeout: String,

    /// Number of concurrent upload requests (1 through 4)
    #[arg(long, default_value_t = DEFAULT_CLIENT_DEVNULL_CONCURRENCY)]
    pub concurrency: u8,

    /// Confirm generation of deliberate network load
    #[arg(long, required = true)]
    pub yes: bool,
}

pub(super) async fn execute_client_devnull(
    args: ClientDevnullArgs,
    formatter: &Formatter,
) -> ExitCode {
    let request = match validate_client_devnull_args(&args) {
        Ok(request) => request,
        Err(message) => return emit_code_error(ExitCode::UsageError, message, formatter),
    };
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    run_client_devnull(
        &args.alias,
        request,
        &client,
        formatter,
        tokio::signal::ctrl_c(),
    )
    .await
}

async fn run_client_devnull<F>(
    alias: &str,
    request: ClientDevnullRequest,
    api: &dyn DiagnosticApi,
    formatter: &Formatter,
    interrupt: F,
) -> ExitCode
where
    F: Future<Output = std::io::Result<()>>,
{
    tokio::pin!(interrupt);
    let result = tokio::select! {
        biased;
        interrupt_result = &mut interrupt => {
            return match interrupt_result {
                Ok(()) => emit_code_error(
                    ExitCode::Interrupted,
                    "Client devnull probe was interrupted".to_string(),
                    formatter,
                ),
                Err(error) => emit_code_error(
                    ExitCode::GeneralError,
                    format!("Failed to register Ctrl-C handler for client devnull probe: {error}"),
                    formatter,
                ),
            };
        }
        result = api.client_devnull(request) => result,
    };

    match result {
        Ok(result) => {
            if formatter.is_json() {
                formatter.json(&success_output(alias, result));
            } else {
                for line in human_result_lines(alias, result, formatter) {
                    formatter.println(&line);
                }
            }
            ExitCode::Success
        }
        Err(error) => emit_probe_error(&error, formatter),
    }
}

fn validate_client_devnull_args(args: &ClientDevnullArgs) -> Result<ClientDevnullRequest, String> {
    if !args.yes {
        return Err("Client devnull requires --yes because it generates network load".to_string());
    }
    let bytes = parse_byte_size(&args.size)?;
    let timeout = parse_timeout(&args.timeout)?;
    ClientDevnullRequest::new(bytes, args.concurrency, timeout).map_err(|error| error.to_string())
}

fn parse_byte_size(value: &str) -> Result<u64, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("Client devnull size cannot be empty".to_string());
    }
    let split_index = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split_index);
    if number.is_empty() {
        return Err(format!("Invalid client devnull size: '{value}'"));
    }
    let number = number
        .parse::<u64>()
        .map_err(|_| format!("Invalid client devnull size number: '{number}'"))?;
    let multiplier = match unit.to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1024,
        "M" | "MB" | "MIB" => 1024 * 1024,
        "G" | "GB" | "GIB" => 1024 * 1024 * 1024,
        _ => return Err(format!("Invalid client devnull size unit: '{unit}'")),
    };
    number
        .checked_mul(multiplier)
        .ok_or_else(|| format!("Client devnull size is too large: '{value}'"))
}

fn parse_timeout(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let seconds = value
        .strip_suffix('s')
        .or_else(|| value.strip_suffix('S'))
        .ok_or_else(|| "Client devnull timeout must use whole seconds such as 30s".to_string())?;
    if seconds.is_empty() || !seconds.chars().all(|character| character.is_ascii_digit()) {
        return Err(format!("Invalid client devnull timeout: '{value}'"));
    }
    let seconds = seconds
        .parse::<u64>()
        .map_err(|_| format!("Client devnull timeout is too large: '{value}'"))?;
    Ok(Duration::from_secs(seconds))
}

fn emit_probe_error(error: &Error, formatter: &Formatter) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    emit_error(
        code,
        format!("Client devnull probe failed: {error}"),
        matches!(error, Error::UnsupportedFeature(_)),
        formatter,
    )
}

fn emit_code_error(code: ExitCode, message: String, formatter: &Formatter) -> ExitCode {
    emit_error(code, message, false, formatter)
}

fn emit_error(
    code: ExitCode,
    message: String,
    unsupported: bool,
    formatter: &Formatter,
) -> ExitCode {
    if formatter.is_json() {
        formatter.json_error(&error_output(code, message, unsupported));
    } else {
        formatter.error_with_code(code, &message);
    }
    code
}

fn human_result_lines(
    alias: &str,
    result: ClientDevnullResult,
    formatter: &Formatter,
) -> Vec<String> {
    vec![
        format!(
            "Client-to-server devnull throughput ({})",
            formatter.sanitize_text(alias)
        ),
        format!(
            "Requested: {}",
            humansize::format_size(result.requested_bytes, humansize::BINARY)
        ),
        format!(
            "Received: {}",
            humansize::format_size(result.received_bytes, humansize::BINARY)
        ),
        format!("Concurrency: {}", result.concurrency),
        format!("Elapsed: {:.3} s", result.elapsed_seconds),
        format!(
            "Aggregate upload throughput: {:.2} MiB/s",
            result.aggregate_throughput_bytes_per_second / (1024.0 * 1024.0)
        ),
    ]
}

#[derive(Debug, Serialize)]
struct AdminOperationsSuccessOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: AdminOperationsData,
}

#[derive(Debug, Serialize)]
struct AdminOperationsData {
    operations: Vec<AdminOperationOutput>,
}

#[derive(Debug, Serialize)]
struct AdminOperationOutput {
    operation: &'static str,
    resource: String,
    state: &'static str,
    operation_id: Option<String>,
    changed: bool,
    result: ClientDevnullOutput,
}

#[derive(Debug, Serialize)]
struct ClientDevnullOutput {
    direction: &'static str,
    requested_bytes: u64,
    received_bytes: u64,
    concurrency: u8,
    elapsed_seconds: f64,
    aggregate_throughput_bytes_per_second: f64,
}

fn success_output(alias: &str, result: ClientDevnullResult) -> AdminOperationsSuccessOutput {
    AdminOperationsSuccessOutput {
        schema_version: 3,
        output_type: "admin_operations",
        status: "success",
        data: AdminOperationsData {
            operations: vec![AdminOperationOutput {
                operation: "diagnostics_client_devnull",
                resource: alias.to_string(),
                state: "succeeded",
                operation_id: None,
                changed: false,
                result: ClientDevnullOutput {
                    direction: "client-to-server",
                    requested_bytes: result.requested_bytes,
                    received_bytes: result.received_bytes,
                    concurrency: result.concurrency,
                    elapsed_seconds: result.elapsed_seconds,
                    aggregate_throughput_bytes_per_second: result
                        .aggregate_throughput_bytes_per_second,
                },
            }],
        },
    }
}

#[derive(Debug, Serialize)]
struct AdminOperationsErrorOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    error: ProbeErrorOutput,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ProbeErrorOutput {
    Unsupported(UnsupportedProbeError),
    Standard(StandardProbeError),
}

#[derive(Debug, Serialize)]
struct UnsupportedProbeError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    capability: &'static str,
    server: Option<String>,
    suggestion: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct StandardProbeError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    suggestion: Option<&'static str>,
}

fn error_output(code: ExitCode, message: String, unsupported: bool) -> AdminOperationsErrorOutput {
    let error = if unsupported {
        ProbeErrorOutput::Unsupported(UnsupportedProbeError {
            error_type: "unsupported_feature",
            message,
            retryable: false,
            capability: CLIENT_DEVNULL_CAPABILITY,
            server: None,
            suggestion: Some("Use a RustFS release with a measured client-devnull implementation."),
        })
    } else {
        let (error_type, retryable, suggestion) = standard_error_metadata(code);
        ProbeErrorOutput::Standard(StandardProbeError {
            error_type,
            message,
            retryable,
            suggestion,
        })
    };
    AdminOperationsErrorOutput {
        schema_version: 3,
        output_type: "admin_operations",
        status: "error",
        error,
    }
}

const fn standard_error_metadata(code: ExitCode) -> (&'static str, bool, Option<&'static str>) {
    match code {
        ExitCode::UsageError => (
            "usage_error",
            false,
            Some("Review --size, --timeout, --concurrency, and the required --yes flag."),
        ),
        ExitCode::NetworkError => (
            "network_error",
            true,
            Some("Verify connectivity and retry the bounded probe when safe."),
        ),
        ExitCode::AuthError => (
            "auth_error",
            false,
            Some("Verify credentials and the HealthInfoAdminAction permission."),
        ),
        ExitCode::Interrupted => (
            "interrupted",
            true,
            Some("Retry the bounded probe when network load is safe."),
        ),
        ExitCode::NotFound => ("not_found", false, None),
        ExitCode::Conflict => ("conflict", false, None),
        ExitCode::Success | ExitCode::GeneralError | ExitCode::UnsupportedFeature => {
            ("general_error", false, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::{pending, ready};
    use std::io;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use jsonschema::Validator;
    use rc_core::Result;
    use rc_core::admin::{
        CapabilityApi, CapabilityReport, DEFAULT_CLIENT_DEVNULL_BYTES,
        DEFAULT_CLIENT_DEVNULL_TIMEOUT,
    };
    use serde_json::Value;

    use super::*;
    use crate::output::OutputConfig;

    #[derive(Clone, Copy)]
    enum StubOutcome {
        Success,
        Auth,
        Network,
        Unsupported,
        Pending,
    }

    struct DropMarker(Arc<AtomicBool>);

    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct StubDiagnosticApi {
        outcome: StubOutcome,
        calls: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
    }

    #[async_trait]
    impl CapabilityApi for StubDiagnosticApi {
        async fn discover_capabilities(&self, _refresh: bool) -> Result<CapabilityReport> {
            Err(Error::General("not used by the command layer".to_string()))
        }
    }

    #[async_trait]
    impl DiagnosticApi for StubDiagnosticApi {
        async fn client_devnull(
            &self,
            _request: ClientDevnullRequest,
        ) -> Result<ClientDevnullResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.outcome {
                StubOutcome::Success => Ok(result()),
                StubOutcome::Auth => Err(Error::Auth("Access denied".to_string())),
                StubOutcome::Network => Err(Error::Network("Probe timed out".to_string())),
                StubOutcome::Unsupported => {
                    Err(Error::UnsupportedFeature("Probe is stubbed".to_string()))
                }
                StubOutcome::Pending => {
                    let _drop_marker = DropMarker(Arc::clone(&self.dropped));
                    pending::<()>().await;
                    Err(Error::General("pending probe completed".to_string()))
                }
            }
        }
    }

    fn api(outcome: StubOutcome) -> StubDiagnosticApi {
        StubDiagnosticApi {
            outcome,
            calls: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicBool::new(false)),
        }
    }

    fn args() -> ClientDevnullArgs {
        ClientDevnullArgs {
            alias: "local".to_string(),
            size: "8MiB".to_string(),
            timeout: "30s".to_string(),
            concurrency: 1,
            yes: true,
        }
    }

    fn request() -> ClientDevnullRequest {
        validate_client_devnull_args(&args()).expect("default arguments should be valid")
    }

    fn result() -> ClientDevnullResult {
        ClientDevnullResult {
            requested_bytes: DEFAULT_CLIENT_DEVNULL_BYTES,
            received_bytes: DEFAULT_CLIENT_DEVNULL_BYTES,
            concurrency: 1,
            elapsed_seconds: 0.5,
            aggregate_throughput_bytes_per_second: 16_777_216.0,
        }
    }

    fn formatter() -> Formatter {
        Formatter::new(OutputConfig {
            quiet: true,
            ..OutputConfig::default()
        })
    }

    fn output_v3_validator() -> Validator {
        let schema_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("schemas/output_v3.json");
        let schema = std::fs::read_to_string(&schema_path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", schema_path.display()));
        let schema: Value = serde_json::from_str(&schema).expect("output v3 schema should parse");
        jsonschema::validator_for(&schema).expect("output v3 schema should compile")
    }

    #[test]
    fn client_devnull_argument_defaults_and_limits_are_validated() {
        let request = validate_client_devnull_args(&args()).expect("defaults should be valid");
        assert_eq!(request.bytes_per_request(), DEFAULT_CLIENT_DEVNULL_BYTES);
        assert_eq!(request.timeout(), DEFAULT_CLIENT_DEVNULL_TIMEOUT);
        assert_eq!(request.concurrency(), DEFAULT_CLIENT_DEVNULL_CONCURRENCY);

        let mut invalid = args();
        invalid.yes = false;
        assert!(validate_client_devnull_args(&invalid).is_err());
        invalid = args();
        invalid.size = "32MiB".to_string();
        invalid.concurrency = 3;
        assert!(validate_client_devnull_args(&invalid).is_err());
        invalid = args();
        invalid.timeout = "61s".to_string();
        assert!(validate_client_devnull_args(&invalid).is_err());
    }

    #[test]
    fn client_devnull_success_output_satisfies_output_v3() {
        let value = serde_json::to_value(success_output("local", result()))
            .expect("success output should serialize");
        assert_eq!(value["type"], "admin_operations");
        assert_eq!(
            value["data"]["operations"][0]["result"]["direction"],
            "client-to-server"
        );
        assert_eq!(
            value["data"]["operations"][0]["result"]["requested_bytes"],
            DEFAULT_CLIENT_DEVNULL_BYTES
        );
        assert!(output_v3_validator().is_valid(&value));
    }

    #[test]
    fn client_devnull_human_output_contains_required_measurements() {
        let lines = human_result_lines("local", result(), &formatter()).join("\n");
        for expected in [
            "Client-to-server",
            "Requested:",
            "Received:",
            "Concurrency: 1",
            "Elapsed: 0.500 s",
            "Aggregate upload throughput:",
        ] {
            assert!(lines.contains(expected), "missing {expected}: {lines}");
        }
    }

    #[tokio::test]
    async fn client_devnull_preserves_auth_timeout_and_unsupported_exit_codes() {
        for (outcome, expected) in [
            (StubOutcome::Auth, ExitCode::AuthError),
            (StubOutcome::Network, ExitCode::NetworkError),
            (StubOutcome::Unsupported, ExitCode::UnsupportedFeature),
        ] {
            let api = api(outcome);
            let code = run_client_devnull(
                "local",
                request(),
                &api,
                &formatter(),
                pending::<io::Result<()>>(),
            )
            .await;
            assert_eq!(code, expected);
            assert_eq!(api.calls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn client_devnull_interrupt_cancels_in_flight_probe() {
        let api = api(StubOutcome::Pending);
        let dropped = Arc::clone(&api.dropped);

        let interrupt = async {
            tokio::task::yield_now().await;
            Ok(())
        };
        let code = run_client_devnull("local", request(), &api, &formatter(), interrupt).await;

        assert_eq!(code, ExitCode::Interrupted);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn client_devnull_ready_interrupt_returns_interrupted_without_starting_probe() {
        let api = api(StubOutcome::Pending);

        let code = run_client_devnull("local", request(), &api, &formatter(), ready(Ok(()))).await;

        assert_eq!(code, ExitCode::Interrupted);
        assert_eq!(api.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn client_devnull_interrupt_setup_error_returns_general_error() {
        let api = api(StubOutcome::Pending);

        let code = run_client_devnull(
            "local",
            request(),
            &api,
            &formatter(),
            ready(Err(io::Error::other("signal registration failed"))),
        )
        .await;

        assert_eq!(code, ExitCode::GeneralError);
        assert_eq!(api.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn client_devnull_pending_interrupt_allows_success() {
        let api = api(StubOutcome::Success);
        let code = run_client_devnull(
            "local",
            request(),
            &api,
            &formatter(),
            pending::<io::Result<()>>(),
        )
        .await;
        assert_eq!(code, ExitCode::Success);
    }

    #[test]
    fn client_devnull_error_outputs_satisfy_output_v3() {
        let validator = output_v3_validator();
        for (code, unsupported) in [
            (ExitCode::UsageError, false),
            (ExitCode::NetworkError, false),
            (ExitCode::AuthError, false),
            (ExitCode::Interrupted, false),
            (ExitCode::UnsupportedFeature, true),
        ] {
            let value =
                serde_json::to_value(error_output(code, "probe failed".to_string(), unsupported))
                    .expect("error output should serialize");
            assert!(validator.is_valid(&value), "invalid output: {value}");
        }
    }
}
