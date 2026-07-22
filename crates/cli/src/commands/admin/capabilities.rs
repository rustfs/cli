//! Runtime capability discovery command.

use clap::Args;
use rc_core::Error;
use rc_core::admin::{CapabilityApi, CapabilityAvailability, CapabilityReport};
use serde::Serialize;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;

/// Inspect the effective RustFS Admin API capabilities.
#[derive(Args, Debug)]
pub struct CapabilitiesArgs {
    /// Alias name of the server
    pub alias: String,

    /// Bypass the in-process capability cache
    #[arg(long)]
    pub refresh: bool,
}

/// Execute runtime capability discovery.
pub async fn execute(args: CapabilitiesArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    execute_with_api(args.refresh, &client, formatter).await
}

async fn execute_with_api(
    refresh: bool,
    api: &dyn CapabilityApi,
    formatter: &Formatter,
) -> ExitCode {
    match api.discover_capabilities(refresh).await {
        Ok(report) => {
            if formatter.is_json() {
                formatter.json(&success_output(&report));
            } else {
                print_report(&report, formatter);
            }
            ExitCode::Success
        }
        Err(error) => emit_capability_error(&error, "Failed to discover capabilities", formatter),
    }
}

fn emit_capability_error(error: &Error, context: &str, formatter: &Formatter) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    let message = format!("{context}: {error}");
    if formatter.is_json() {
        formatter.json_error(&error_output(error, code, message));
    } else {
        formatter.error_with_code(code, &message);
    }
    code
}

fn print_report(report: &CapabilityReport, formatter: &Formatter) {
    for line in report_lines(report, formatter) {
        formatter.println(&line);
    }
}

fn report_lines(report: &CapabilityReport, formatter: &Formatter) -> Vec<String> {
    let version = formatter.sanitize_text(report.server_version.as_deref().unwrap_or("unknown"));
    let mut lines = vec![
        format!("RustFS capabilities ({version})"),
        String::new(),
        "CAPABILITY                         STATUS             REASON".to_string(),
    ];
    for capability in &report.capabilities {
        let name = formatter.sanitize_text(&capability.name);
        let reason = formatter.sanitize_text(capability.reason.as_deref().unwrap_or("-"));
        lines.push(format!(
            "{:<34} {:<18} {}",
            name, capability.availability, reason
        ));
    }

    if !report.extensions.is_empty() {
        lines.push(String::new());
        lines.push(format!("Extensions: {}", report.extensions.len()));
    }

    lines
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct CapabilitiesSuccessOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: CapabilitiesData,
    meta: CapabilitiesMeta,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct CapabilitiesData {
    server: &'static str,
    api_version: Option<&'static str>,
    capabilities: Vec<CapabilityOutput>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct CapabilityOutput {
    name: String,
    scope: &'static str,
    support: &'static str,
    reason: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct CapabilitiesMeta {
    server_version: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct CapabilitiesErrorOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    error: CapabilityErrorOutput,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(untagged)]
enum CapabilityErrorOutput {
    Unsupported(UnsupportedCapabilityError),
    Standard(StandardCapabilityError),
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct UnsupportedCapabilityError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    capability: &'static str,
    server: Option<String>,
    suggestion: Option<&'static str>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct StandardCapabilityError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    suggestion: Option<&'static str>,
}

fn success_output(report: &CapabilityReport) -> CapabilitiesSuccessOutput {
    let api_version = !report.capabilities.iter().any(|capability| {
        capability.name == "admin.runtime-capabilities"
            && capability.availability == CapabilityAvailability::VersionGated
    });
    let capabilities = report
        .capabilities
        .iter()
        .map(|capability| CapabilityOutput {
            name: capability.name.clone(),
            scope: capability_scope(&capability.name),
            support: capability_support(capability.availability),
            reason: capability.reason.clone(),
        })
        .collect();

    CapabilitiesSuccessOutput {
        schema_version: 3,
        output_type: "capabilities",
        status: "success",
        data: CapabilitiesData {
            server: "rustfs",
            api_version: api_version.then_some("v4"),
            capabilities,
        },
        meta: CapabilitiesMeta {
            server_version: report.server_version.clone(),
        },
    }
}

fn capability_scope(name: &str) -> &'static str {
    if name.starts_with("runtime.") {
        "runtime"
    } else if name.starts_with("admin.") {
        "admin"
    } else {
        "server"
    }
}

const fn capability_support(availability: CapabilityAvailability) -> &'static str {
    match availability {
        CapabilityAvailability::Available => "supported",
        CapabilityAvailability::Stubbed => "stub",
        CapabilityAvailability::Unsupported => "unsupported",
        CapabilityAvailability::Disabled => "disabled",
        CapabilityAvailability::VersionGated => "unsupported",
        CapabilityAvailability::PermissionDenied | CapabilityAvailability::Unknown => "unknown",
    }
}

fn error_output(error: &Error, code: ExitCode, message: String) -> CapabilitiesErrorOutput {
    let error = if matches!(error, Error::UnsupportedFeature(_)) {
        CapabilityErrorOutput::Unsupported(UnsupportedCapabilityError {
            error_type: "unsupported_feature",
            message,
            retryable: false,
            capability: "runtime_capabilities",
            server: None,
            suggestion: Some("Upgrade RustFS or retry after verifying server capability support."),
        })
    } else {
        let (error_type, retryable, suggestion) = standard_error_metadata(code);
        CapabilityErrorOutput::Standard(StandardCapabilityError {
            error_type,
            message,
            retryable,
            suggestion,
        })
    };

    CapabilitiesErrorOutput {
        schema_version: 3,
        output_type: "capabilities",
        status: "error",
        error,
    }
}

const fn standard_error_metadata(code: ExitCode) -> (&'static str, bool, Option<&'static str>) {
    match code {
        ExitCode::UsageError => (
            "usage_error",
            false,
            Some("Run the command with --help and verify its arguments."),
        ),
        ExitCode::NetworkError => (
            "network_error",
            true,
            Some("Verify the endpoint and network connectivity, then retry."),
        ),
        ExitCode::AuthError => (
            "auth_error",
            false,
            Some("Verify the alias credentials and ServerInfoAdminAction permission."),
        ),
        ExitCode::NotFound => (
            "not_found",
            false,
            Some("Verify that the alias and server endpoint exist."),
        ),
        ExitCode::Conflict => (
            "conflict",
            false,
            Some("Review the server state and retry."),
        ),
        ExitCode::Interrupted => (
            "interrupted",
            true,
            Some("Retry the command if capability discovery is still needed."),
        ),
        ExitCode::Success | ExitCode::GeneralError | ExitCode::UnsupportedFeature => {
            ("general_error", false, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use jsonschema::Validator;
    use rc_core::Result;
    use rc_core::admin::{
        CapabilityAvailability, CapabilityEntry, ClusterSnapshotMetadata, ExtensionMetadata,
    };
    use serde_json::Value;
    use std::path::Path;

    use super::*;
    use crate::output::OutputConfig;

    const SUCCESS_GOLDEN: &str =
        include_str!("../../../tests/fixtures/output_v3/capabilities/runtime_success.json");
    const VERSION_GATED_GOLDEN: &str =
        include_str!("../../../tests/fixtures/output_v3/capabilities/version_gated.json");
    const RUNTIME_UNSUPPORTED_GOLDEN: &str =
        include_str!("../../../tests/fixtures/output_v3/capabilities/runtime_unsupported.json");
    const AUTH_ERROR_GOLDEN: &str =
        include_str!("../../../tests/fixtures/output_v3/capabilities/auth_error.json");

    struct StubCapabilityApi {
        result: Result<CapabilityReport>,
    }

    #[async_trait]
    impl CapabilityApi for StubCapabilityApi {
        async fn discover_capabilities(&self, _refresh: bool) -> Result<CapabilityReport> {
            match &self.result {
                Ok(report) => Ok(report.clone()),
                Err(Error::Auth(message)) => Err(Error::Auth(message.clone())),
                Err(Error::UnsupportedFeature(message)) => {
                    Err(Error::UnsupportedFeature(message.clone()))
                }
                Err(error) => Err(Error::General(error.to_string())),
            }
        }
    }

    fn report() -> CapabilityReport {
        CapabilityReport {
            server_version: Some("1.0.0-beta.10".to_string()),
            runtime_path: "/rustfs/admin/v4/runtime/capabilities".to_string(),
            extensions_path: "/rustfs/admin/v4/extensions/catalog".to_string(),
            cluster_snapshot_path: "/rustfs/admin/v4/cluster/snapshot".to_string(),
            capabilities: vec![CapabilityEntry {
                name: "runtime.observability".to_string(),
                availability: CapabilityAvailability::Available,
                reason: None,
            }],
            extensions: Vec::<ExtensionMetadata>::new(),
            cluster: ClusterSnapshotMetadata {
                summary: None,
                runtime_capabilities_path: None,
                extensions_catalog_path: None,
            },
        }
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

    fn assert_valid_v3(value: &Value) {
        let validator = output_v3_validator();
        let errors = validator
            .iter_errors(value)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert!(
            errors.is_empty(),
            "capability output must satisfy output v3:\n{}",
            errors.join("\n")
        );
    }

    fn golden(contents: &str) -> Value {
        serde_json::from_str(contents).expect("capability golden fixture should parse")
    }

    #[tokio::test]
    async fn command_returns_success_for_discovery_report() {
        let formatter = Formatter::new(OutputConfig {
            quiet: true,
            ..Default::default()
        });
        let api = StubCapabilityApi {
            result: Ok(report()),
        };

        assert_eq!(
            execute_with_api(false, &api, &formatter).await,
            ExitCode::Success
        );
    }

    #[tokio::test]
    async fn command_preserves_permission_denial_exit_code() {
        let formatter = Formatter::new(OutputConfig {
            json: true,
            ..Default::default()
        });
        let api = StubCapabilityApi {
            result: Err(Error::Auth("Access denied".to_string())),
        };

        assert_eq!(
            execute_with_api(false, &api, &formatter).await,
            ExitCode::AuthError
        );
    }

    #[tokio::test]
    async fn command_preserves_unsupported_feature_exit_code() {
        let formatter = Formatter::new(OutputConfig {
            json: true,
            ..Default::default()
        });
        let api = StubCapabilityApi {
            result: Err(Error::UnsupportedFeature("NotImplemented".to_string())),
        };

        assert_eq!(
            execute_with_api(false, &api, &formatter).await,
            ExitCode::UnsupportedFeature
        );
    }

    #[test]
    fn success_json_uses_capabilities_output_v3_contract() {
        let value = serde_json::to_value(success_output(&report()))
            .expect("capability success output should serialize");

        assert_eq!(value, golden(SUCCESS_GOLDEN));
        assert_valid_v3(&value);
    }

    #[test]
    fn version_gated_json_is_a_v3_unsupported_capability() {
        let mut report = report();
        report.capabilities = vec![CapabilityEntry {
            name: "admin.runtime-capabilities".to_string(),
            availability: CapabilityAvailability::VersionGated,
            reason: Some("Admin API v4 is unavailable".to_string()),
        }];
        let value = serde_json::to_value(success_output(&report))
            .expect("version-gated output should serialize");

        assert_eq!(value["data"]["api_version"], Value::Null);
        assert_eq!(value["data"]["capabilities"][0]["support"], "unsupported");
        assert_eq!(value, golden(VERSION_GATED_GOLDEN));
        assert_valid_v3(&value);
    }

    #[test]
    fn runtime_unsupported_json_is_distinct_from_a_stub() {
        let mut report = report();
        report.capabilities = vec![CapabilityEntry {
            name: "runtime.memory-sampling".to_string(),
            availability: CapabilityAvailability::Unsupported,
            reason: Some("not available on this platform".to_string()),
        }];
        let value = serde_json::to_value(success_output(&report))
            .expect("runtime unsupported output should serialize");

        assert_eq!(value["data"]["api_version"], "v4");
        assert_eq!(value["data"]["capabilities"][0]["support"], "unsupported");
        assert_ne!(value["data"]["capabilities"][0]["support"], "stub");
        assert_eq!(value, golden(RUNTIME_UNSUPPORTED_GOLDEN));
        assert_valid_v3(&value);
    }

    #[test]
    fn discovery_error_json_uses_capabilities_output_v3_contract() {
        let error = Error::Auth("Access denied".to_string());
        let value = serde_json::to_value(error_output(
            &error,
            ExitCode::AuthError,
            format!("Failed to discover capabilities: {error}"),
        ))
        .expect("capability error output should serialize");

        assert_eq!(value["schema_version"], 3);
        assert_eq!(value["type"], "capabilities");
        assert_eq!(value["status"], "error");
        assert_eq!(value["error"]["type"], "auth_error");
        assert_eq!(value["error"]["retryable"], false);
        assert_eq!(value, golden(AUTH_ERROR_GOLDEN));
        assert_valid_v3(&value);
    }

    #[test]
    fn unsupported_error_json_uses_specialized_v3_contract() {
        let error = Error::UnsupportedFeature("NotImplemented".to_string());
        let value = serde_json::to_value(error_output(
            &error,
            ExitCode::UnsupportedFeature,
            format!("Failed to discover capabilities: {error}"),
        ))
        .expect("unsupported capability error should serialize");

        assert_eq!(value["error"]["type"], "unsupported_feature");
        assert_eq!(value["error"]["capability"], "runtime_capabilities");
        assert_eq!(value["error"]["server"], Value::Null);
        assert_valid_v3(&value);
    }

    #[test]
    fn table_output_sanitizes_server_controlled_strings() {
        let mut report = report();
        report.server_version = Some("beta.10\n\u{1b}[31m".to_string());
        report.capabilities[0].name = "runtime.bad\nname\u{1b}[2J".to_string();
        report.capabilities[0].reason = Some("line one\r\nline two\u{1b}[0m".to_string());
        let formatter = Formatter::new(OutputConfig {
            no_color: true,
            ..Default::default()
        });

        let lines = report_lines(&report, &formatter);

        assert!(lines.iter().all(|line| !line.contains('\u{1b}')));
        assert!(lines.iter().all(|line| !line.contains('\r')));
        assert!(lines.iter().all(|line| !line.contains('\n')));
        assert!(lines.iter().any(|line| line.contains("beta.10\\n\\u{1b}")));
        assert!(lines.iter().any(|line| line.contains("runtime.bad\\nname")));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("line one\\r\\nline two"))
        );
    }
}
