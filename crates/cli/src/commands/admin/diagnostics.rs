//! RustFS read-only snapshots and explicitly confirmed bounded active diagnostics.

mod client_devnull;

use std::collections::BTreeMap;

use clap::Subcommand;
use rc_core::Error;
use rc_core::admin::{
    CapabilityApi, CapabilityAvailability, ClusterSnapshotDocument, DetailedHealthSnapshot,
    DiagnosticCapability, DiagnosticReadApi, ExtensionsCatalog,
};
use serde::Serialize;
use serde_json::Value;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;

pub use client_devnull::ClientDevnullArgs;

#[derive(Subcommand, Debug, Clone)]
pub enum DiagnosticsCommands {
    /// Display authenticated host, process, and drive health observations
    Health(DiagnosticsArgs),
    /// Display the read-only RustFS cluster snapshot
    Cluster(DiagnosticsArgs),
    /// Display extension schemas and runtime capability summaries
    Extensions(DiagnosticsArgs),
    /// Measure bounded client-to-server upload throughput without storing data
    #[command(name = "client-devnull")]
    ClientDevnull(ClientDevnullArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct DiagnosticsArgs {
    /// Alias name of the RustFS server
    pub alias: String,
}

impl DiagnosticsCommands {
    #[cfg(test)]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Health(_) => "health",
            Self::Cluster(_) => "cluster",
            Self::Extensions(_) => "extensions",
            Self::ClientDevnull(_) => "client-devnull",
        }
    }

    pub fn alias(&self) -> &str {
        match self {
            Self::Health(args) | Self::Cluster(args) | Self::Extensions(args) => &args.alias,
            Self::ClientDevnull(args) => &args.alias,
        }
    }

    const fn capability(&self) -> DiagnosticCapability {
        match self {
            Self::Health(_) => DiagnosticCapability::HealthSnapshot,
            Self::Cluster(_) => DiagnosticCapability::ClusterSnapshot,
            Self::Extensions(_) => DiagnosticCapability::ExtensionsCatalog,
            Self::ClientDevnull(_) => DiagnosticCapability::ClientDevnull,
        }
    }

    const fn output_type(&self) -> &'static str {
        match self {
            Self::Health(_) => "health_snapshot",
            Self::Cluster(_) => "cluster_snapshot",
            Self::Extensions(_) => "extension_catalog",
            Self::ClientDevnull(_) => "admin_operations",
        }
    }
}

#[derive(Debug, Serialize)]
struct SuccessOutput<T> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: T,
}

#[derive(Debug, Serialize)]
struct ClusterSnapshotOutput {
    available: bool,
    state: &'static str,
    snapshot: Option<rc_core::admin::DiagnosticClusterSnapshot>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Serialize)]
struct ErrorOutput<'a> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'a str,
    status: &'static str,
    error: ErrorBody<'a>,
}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    capability: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<&'static str>,
    suggestion: Option<&'static str>,
}

pub async fn execute(command: DiagnosticsCommands, formatter: &Formatter) -> ExitCode {
    match command {
        DiagnosticsCommands::ClientDevnull(args) => {
            client_devnull::execute_client_devnull(args, formatter).await
        }
        read_command => {
            let client = match get_admin_client(read_command.alias(), formatter) {
                Ok(client) => client,
                Err(code) => return code,
            };
            execute_with_api(read_command, &client, &client, formatter).await
        }
    }
}

async fn execute_with_api(
    command: DiagnosticsCommands,
    capabilities: &dyn CapabilityApi,
    diagnostics: &dyn DiagnosticReadApi,
    formatter: &Formatter,
) -> ExitCode {
    let report = match capabilities.discover_capabilities(false).await {
        Ok(report) => report,
        Err(error) => {
            return emit_diagnostic_error(
                &command,
                "Failed to discover capabilities",
                &error,
                formatter,
            );
        }
    };
    if let Err(error) = report.require_diagnostic_capability(command.capability()) {
        let error = match error.availability() {
            CapabilityAvailability::PermissionDenied => Error::Auth(error.to_string()),
            _ => Error::UnsupportedFeature(error.to_string()),
        };
        return emit_diagnostic_error(
            &command,
            "Diagnostic capability is unavailable",
            &error,
            formatter,
        );
    }

    match &command {
        DiagnosticsCommands::Health(_) => match diagnostics.health_snapshot().await {
            Ok(snapshot) => output_health(snapshot, formatter),
            Err(error) => emit_diagnostic_error(
                &command,
                "Failed to read detailed health snapshot",
                &error,
                formatter,
            ),
        },
        DiagnosticsCommands::Cluster(_) => match diagnostics.cluster_snapshot().await {
            Ok(document) => output_cluster(document, formatter),
            Err(error) => emit_diagnostic_error(
                &command,
                "Failed to read cluster snapshot",
                &error,
                formatter,
            ),
        },
        DiagnosticsCommands::Extensions(_) => match diagnostics.extensions_catalog().await {
            Ok(catalog) => output_extensions(catalog, formatter),
            Err(error) => emit_diagnostic_error(
                &command,
                "Failed to read extension catalog",
                &error,
                formatter,
            ),
        },
        DiagnosticsCommands::ClientDevnull(_) => emit_diagnostic_error(
            &command,
            "Failed to route active diagnostic",
            &Error::General(
                "Client devnull must use the bounded active diagnostic executor".to_string(),
            ),
            formatter,
        ),
    }
}

fn emit_diagnostic_error(
    command: &DiagnosticsCommands,
    context: &str,
    error: &Error,
    formatter: &Formatter,
) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    let message = format!("{context}: {error}");
    if formatter.is_json() {
        let unsupported = matches!(error, Error::UnsupportedFeature(_));
        formatter.json_error(&ErrorOutput {
            schema_version: 3,
            output_type: command.output_type(),
            status: "error",
            error: ErrorBody {
                error_type: diagnostic_error_type(code),
                message,
                retryable: code == ExitCode::NetworkError,
                capability: unsupported.then_some(command.capability().name()),
                server: unsupported.then_some("rustfs"),
                suggestion: diagnostic_error_suggestion(code),
            },
        });
    } else {
        formatter.error_with_code(code, &message);
    }
    code
}

const fn diagnostic_error_type(code: ExitCode) -> &'static str {
    match code {
        ExitCode::UsageError => "usage_error",
        ExitCode::NetworkError => "network_error",
        ExitCode::AuthError => "auth_error",
        ExitCode::NotFound => "not_found",
        ExitCode::Conflict => "conflict",
        ExitCode::UnsupportedFeature => "unsupported_feature",
        ExitCode::Interrupted => "interrupted",
        ExitCode::Success | ExitCode::GeneralError => "general_error",
    }
}

const fn diagnostic_error_suggestion(code: ExitCode) -> Option<&'static str> {
    match code {
        ExitCode::NetworkError => Some("Verify the endpoint and network connectivity, then retry."),
        ExitCode::AuthError => Some("Verify credentials and the required RustFS admin permission."),
        ExitCode::UnsupportedFeature => {
            Some("Verify the server diagnostic capability and version.")
        }
        _ => None,
    }
}

fn output_health(snapshot: DetailedHealthSnapshot, formatter: &Formatter) -> ExitCode {
    if formatter.is_json() {
        formatter.json(&health_success_output(snapshot));
    } else {
        print_health(&snapshot, formatter);
    }
    ExitCode::Success
}

fn health_success_output(
    snapshot: DetailedHealthSnapshot,
) -> SuccessOutput<DetailedHealthSnapshot> {
    SuccessOutput {
        schema_version: 3,
        output_type: "health_snapshot",
        status: "success",
        data: snapshot,
    }
}

fn print_health(snapshot: &DetailedHealthSnapshot, formatter: &Formatter) {
    formatter.println(&formatter.style_name("Detailed RustFS health snapshot"));
    formatter.println("Authenticated read-only observations; not mc support diag parity.");
    formatter.println(&format!(
        "Version: {}",
        formatter.sanitize_text(&snapshot.version)
    ));
    formatter.println(&format!(
        "Host: {} ({})",
        formatter.sanitize_text(snapshot.os.hostname.as_deref().unwrap_or("unknown")),
        formatter.sanitize_text(&snapshot.os.arch)
    ));
    formatter.println(&format!(
        "CPU: {} logical cores, {:.2}% observed usage",
        snapshot.cpu.logical_cores, snapshot.cpu.usage_percent
    ));
    formatter.println(&format!(
        "Memory: {} / {} bytes observed",
        snapshot.memory.used_bytes, snapshot.memory.total_bytes
    ));
    formatter.println(&format!(
        "Process: pid {}, {:.2}% CPU, {} bytes memory",
        snapshot.process.pid, snapshot.process.cpu_usage_percent, snapshot.process.memory_bytes
    ));
    formatter.println(&format!("Drives: {}", snapshot.drives.len()));
    for drive in &snapshot.drives {
        formatter.println(&format!(
            "  {} {} state={} read={} write={} latency={}/{} (observed, not benchmark)",
            formatter.sanitize_text(&drive.endpoint),
            formatter.sanitize_text(&drive.drive_path),
            formatter.sanitize_text(&drive.state),
            drive.read_throughput,
            drive.write_throughput,
            drive.read_latency,
            drive.write_latency
        ));
    }
    if !snapshot.unsupported_probes.is_empty() {
        formatter.println("Unsupported probe families:");
        for probe in &snapshot.unsupported_probes {
            formatter.println(&format!("  - {}", formatter.sanitize_text(probe)));
        }
    }
}

fn output_cluster(document: ClusterSnapshotDocument, formatter: &Formatter) -> ExitCode {
    let output = cluster_success_output(document);
    if formatter.is_json() {
        formatter.json(&output);
    } else if let Some(snapshot) = &output.data.snapshot {
        formatter.println(&formatter.style_name("RustFS cluster snapshot"));
        formatter.println(&format!("State: {}", output.data.state));
        formatter.println(&format!(
            "Runtime: {}",
            snapshot.summary.runtime.state_label()
        ));
        formatter.println(&format!(
            "Actionable pressure: {}",
            snapshot.actionable_pressure
        ));
        if let Some(components) = &snapshot.components {
            for (name, condition) in [
                (
                    "storage",
                    components
                        .storage
                        .as_ref()
                        .map(|value| value.condition.as_str()),
                ),
                (
                    "peer-health",
                    components
                        .peer_health
                        .as_ref()
                        .map(|value| value.condition.as_str()),
                ),
                (
                    "listing",
                    components
                        .listing
                        .as_ref()
                        .map(|value| value.condition.as_str()),
                ),
                (
                    "usage",
                    components
                        .usage
                        .as_ref()
                        .map(|value| value.condition.as_str()),
                ),
                (
                    "workload-admission",
                    components
                        .workload_admission
                        .as_ref()
                        .map(|value| value.condition.as_str()),
                ),
            ] {
                if let Some(condition) = condition {
                    formatter.println(&format!("  {name}: {}", formatter.sanitize_text(condition)));
                }
            }
        }
    } else {
        formatter.println(&formatter.style_name("RustFS cluster snapshot"));
        formatter.println("State: initializing_or_unavailable");
        formatter.println("The server returned snapshot: null.");
    }
    ExitCode::Success
}

fn cluster_success_output(
    document: ClusterSnapshotDocument,
) -> SuccessOutput<ClusterSnapshotOutput> {
    let available = document.snapshot.is_some();
    SuccessOutput {
        schema_version: 3,
        output_type: "cluster_snapshot",
        status: "success",
        data: ClusterSnapshotOutput {
            available,
            state: if available {
                "available"
            } else {
                "initializing_or_unavailable"
            },
            snapshot: document.snapshot,
            extra: document.extra,
        },
    }
}

fn output_extensions(mut catalog: ExtensionsCatalog, formatter: &Formatter) -> ExitCode {
    normalize_catalog(&mut catalog);
    if formatter.is_json() {
        formatter.json(&extensions_success_output(catalog));
    } else {
        formatter.println(&formatter.style_name("RustFS extension catalog"));
        formatter.println("Schemas and runtime capability summaries only; instance configuration is not requested.");
        for extension in &catalog.extensions {
            formatter.println(&format!(
                "  {} kind={} version={} capabilities={}",
                formatter.sanitize_text(&extension.extension_id),
                formatter.sanitize_text(&extension.kind),
                formatter.sanitize_text(&extension.version),
                extension.capabilities.len()
            ));
        }
        if !catalog.runtime_capabilities.is_empty() {
            formatter.println("Runtime capability summaries:");
            for (name, capability) in &catalog.runtime_capabilities {
                let extension_id = capability
                    .get("extension_id")
                    .and_then(Value::as_str)
                    .unwrap_or(name);
                let state = capability
                    .get("runtime_capability_summary")
                    .and_then(|summary| summary.get("state"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let reason = capability
                    .get("runtime_capability_summary")
                    .and_then(|summary| summary.get("reason"))
                    .and_then(Value::as_str);
                let suffix = reason.map_or_else(String::new, |reason| {
                    format!(" reason={}", formatter.sanitize_text(reason))
                });
                formatter.println(&format!(
                    "  {} state={}{}",
                    formatter.sanitize_text(extension_id),
                    formatter.sanitize_text(state),
                    suffix
                ));
            }
        }
    }
    ExitCode::Success
}

fn normalize_catalog(catalog: &mut ExtensionsCatalog) {
    catalog
        .extensions
        .sort_by(|left, right| left.extension_id.cmp(&right.extension_id));
    for extension in &mut catalog.extensions {
        extension.capabilities.sort();
    }
}

fn extensions_success_output(catalog: ExtensionsCatalog) -> SuccessOutput<ExtensionsCatalog> {
    SuccessOutput {
        schema_version: 3,
        output_type: "extension_catalog",
        status: "success",
        data: catalog,
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use jsonschema::Validator;
    use rc_core::Result;
    use rc_core::admin::{
        CapabilityEntry, CapabilityReport, ClusterSnapshotMetadata, RuntimeCapabilityState,
        RuntimeCapabilityStatus,
    };
    use serde_json::Value;
    use std::path::Path;

    use super::*;
    use crate::output::OutputConfig;

    #[derive(Clone, Copy)]
    enum ReadResult {
        Success,
        Auth,
        Malformed,
        SemanticallyIncomplete,
    }

    struct StubApi {
        availability: CapabilityAvailability,
        read_result: ReadResult,
    }

    impl StubApi {
        fn report(&self) -> CapabilityReport {
            CapabilityReport {
                server_version: Some("1.0.0-beta.10".to_string()),
                runtime_path: "/rustfs/admin/v4/runtime/capabilities".to_string(),
                extensions_path: "/rustfs/admin/v4/extensions/catalog".to_string(),
                cluster_snapshot_path: "/rustfs/admin/v4/cluster/snapshot".to_string(),
                capabilities: DiagnosticCapability::ALL
                    .into_iter()
                    .map(|capability| CapabilityEntry {
                        name: capability.name().to_string(),
                        availability: self.availability,
                        reason: None,
                    })
                    .collect(),
                extensions: Vec::new(),
                cluster: ClusterSnapshotMetadata {
                    summary: None,
                    runtime_capabilities_path: None,
                    extensions_catalog_path: None,
                },
            }
        }

        fn read_error(&self) -> Option<Error> {
            match self.read_result {
                ReadResult::Success => None,
                ReadResult::Auth => Some(Error::Auth("Access denied".to_string())),
                ReadResult::Malformed => Some(Error::Json(
                    serde_json::from_str::<Value>("{").expect_err("fixture must be malformed JSON"),
                )),
                ReadResult::SemanticallyIncomplete => Some(Error::Json(
                    serde_json::from_value::<DetailedHealthSnapshot>(serde_json::json!({}))
                        .expect_err("empty health envelope must be semantically invalid"),
                )),
            }
        }
    }

    #[async_trait]
    impl CapabilityApi for StubApi {
        async fn discover_capabilities(&self, _refresh: bool) -> Result<CapabilityReport> {
            Ok(self.report())
        }
    }

    #[async_trait]
    impl DiagnosticReadApi for StubApi {
        async fn health_snapshot(&self) -> Result<DetailedHealthSnapshot> {
            match self.read_error() {
                Some(error) => Err(error),
                None => Ok(health()),
            }
        }

        async fn cluster_snapshot(&self) -> Result<ClusterSnapshotDocument> {
            match self.read_error() {
                Some(error) => Err(error),
                None => Ok(unavailable_cluster()),
            }
        }

        async fn extensions_catalog(&self) -> Result<ExtensionsCatalog> {
            match self.read_error() {
                Some(error) => Err(error),
                None => Ok(catalog()),
            }
        }
    }

    fn commands() -> Vec<DiagnosticsCommands> {
        let args = || DiagnosticsArgs {
            alias: "local".to_string(),
        };
        vec![
            DiagnosticsCommands::Health(args()),
            DiagnosticsCommands::Cluster(args()),
            DiagnosticsCommands::Extensions(args()),
        ]
    }

    fn health() -> DetailedHealthSnapshot {
        serde_json::from_value(serde_json::json!({
            "version": "1.0.0-beta.10",
            "cpu": {
                "logical_cores": 8,
                "brand": "test-cpu",
                "frequency_mhz": 2400,
                "usage_percent": 12.5
            },
            "memory": {
                "total_bytes": 1024,
                "used_bytes": 512,
                "available_bytes": 512,
                "total_swap_bytes": 0,
                "used_swap_bytes": 0
            },
            "os": {
                "os": "linux",
                "arch": "x86_64",
                "uptime_secs": 60
            },
            "process": {
                "pid": 42,
                "cpu_usage_percent": 1.25,
                "memory_bytes": 128
            },
            "drives": [],
            "unsupported_probes": ["perf-net"]
        }))
        .expect("health fixture should deserialize")
    }

    fn unavailable_cluster() -> ClusterSnapshotDocument {
        serde_json::from_value(serde_json::json!({"snapshot": null}))
            .expect("null cluster snapshot fixture should deserialize")
    }

    fn catalog() -> ExtensionsCatalog {
        serde_json::from_value(serde_json::json!({
            "extensions": [],
            "runtime_capabilities": {},
            "cluster_snapshot": {},
            "external_plugin_flow": {}
        }))
        .expect("catalog fixture should deserialize")
    }

    fn status(state: RuntimeCapabilityState) -> RuntimeCapabilityStatus {
        RuntimeCapabilityStatus {
            state,
            reason: None,
            extra: BTreeMap::new(),
        }
    }

    fn cluster() -> ClusterSnapshotDocument {
        serde_json::from_value(serde_json::json!({
            "snapshot": {
                "summary": {
                    "runtime": status(RuntimeCapabilityState::Supported),
                    "topology": status(RuntimeCapabilityState::Supported),
                    "membership": status(RuntimeCapabilityState::Supported),
                    "peer_health": status(RuntimeCapabilityState::Supported),
                    "rpc_boundary": status(RuntimeCapabilityState::Supported),
                    "observability": status(RuntimeCapabilityState::Supported),
                    "workload_admission": status(RuntimeCapabilityState::Supported),
                    "actionable_pressure": status(RuntimeCapabilityState::Disabled)
                },
                "runtime_capabilities_path": "/rustfs/admin/v4/runtime/capabilities",
                "extensions_catalog_path": "/rustfs/admin/v4/extensions/catalog",
                "actionable_pressure": false
            }
        }))
        .expect("cluster fixture should deserialize")
    }

    fn validator() -> Validator {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("schemas/output_v3.json");
        let schema: Value = serde_json::from_str(
            &std::fs::read_to_string(path).expect("output schema should be readable"),
        )
        .expect("output schema should parse");
        jsonschema::validator_for(&schema).expect("output schema should compile")
    }

    fn assert_valid(value: &Value) {
        let errors = validator()
            .iter_errors(value)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert!(errors.is_empty(), "invalid output: {}", errors.join("\n"));
    }

    #[tokio::test]
    async fn every_diagnostic_command_has_auth_and_unsupported_exit_scenarios() {
        let formatter = Formatter::new(OutputConfig {
            quiet: true,
            ..Default::default()
        });
        for command in commands() {
            let auth = StubApi {
                availability: CapabilityAvailability::Available,
                read_result: ReadResult::Auth,
            };
            assert_eq!(
                execute_with_api(command.clone(), &auth, &auth, &formatter).await,
                ExitCode::AuthError
            );

            let malformed = StubApi {
                availability: CapabilityAvailability::Available,
                read_result: ReadResult::Malformed,
            };
            assert_eq!(
                execute_with_api(command.clone(), &malformed, &malformed, &formatter).await,
                ExitCode::GeneralError
            );

            let unsupported = StubApi {
                availability: CapabilityAvailability::Unsupported,
                read_result: ReadResult::Success,
            };
            assert_eq!(
                execute_with_api(command, &unsupported, &unsupported, &formatter).await,
                ExitCode::UnsupportedFeature
            );
        }
    }

    #[tokio::test]
    async fn semantically_incomplete_health_payload_maps_to_general_error() {
        let formatter = Formatter::new(OutputConfig {
            quiet: true,
            ..Default::default()
        });
        let api = StubApi {
            availability: CapabilityAvailability::Available,
            read_result: ReadResult::SemanticallyIncomplete,
        };

        assert_eq!(
            execute_with_api(
                DiagnosticsCommands::Health(DiagnosticsArgs {
                    alias: "local".to_string(),
                }),
                &api,
                &api,
                &formatter,
            )
            .await,
            ExitCode::GeneralError
        );
    }

    #[test]
    fn diagnostic_success_outputs_validate_against_v3() {
        let health = serde_json::to_value(health_success_output(health()))
            .expect("health output should serialize");
        let cluster = serde_json::to_value(cluster_success_output(cluster()))
            .expect("cluster output should serialize");
        let unavailable = serde_json::to_value(cluster_success_output(unavailable_cluster()))
            .expect("unavailable cluster output should serialize");
        let extensions = serde_json::to_value(extensions_success_output(catalog()))
            .expect("extension output should serialize");

        for value in [&health, &cluster, &unavailable, &extensions] {
            assert_valid(value);
        }
        assert_eq!(unavailable["data"]["available"], false);
        assert_eq!(unavailable["data"]["snapshot"], Value::Null);

        let document: ClusterSnapshotDocument = serde_json::from_value(serde_json::json!({
            "snapshot": null,
            "future_envelope_field": {"kept": true}
        }))
        .expect("future cluster fixture should deserialize");
        let value = serde_json::to_value(cluster_success_output(document))
            .expect("future cluster output should serialize");
        assert_eq!(value["data"]["future_envelope_field"]["kept"], true);
    }

    #[test]
    fn extension_catalog_output_is_deterministic() {
        let mut catalog: ExtensionsCatalog = serde_json::from_value(serde_json::json!({
            "extensions": [
                {
                    "schema_version": "v1", "extension_id": "z", "display_name": "Z",
                    "provider": "rustfs", "version": "1", "kind": "ops_diagnostics",
                    "capabilities": ["z.capability", "a.capability"], "disabled_by_default": false
                },
                {
                    "schema_version": "v1", "extension_id": "a", "display_name": "A",
                    "provider": "rustfs", "version": "1", "kind": "ops_diagnostics",
                    "capabilities": [], "disabled_by_default": false
                }
            ],
            "runtime_capabilities": {}, "cluster_snapshot": {}, "external_plugin_flow": {}
        }))
        .expect("catalog fixture should deserialize");

        normalize_catalog(&mut catalog);

        assert_eq!(catalog.extensions[0].extension_id, "a");
        assert_eq!(
            catalog.extensions[1].capabilities,
            ["a.capability", "z.capability"]
        );
    }
}
