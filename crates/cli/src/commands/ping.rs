//! Public RustFS liveness probe command.

use std::time::Duration;

use clap::Args;
use rc_core::AliasManager;
use rc_core::ops::{HealthApi, HealthProbe, HealthReport};
use rc_s3::AdminClient;
use serde::Serialize;

use super::ops_output;
use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

const HEALTH_FAMILY: &str = "health";

/// Check service liveness and report round-trip latency.
#[derive(Args, Debug)]
pub struct PingArgs {
    /// Alias name of the service endpoint
    pub alias: String,

    /// Maximum probe duration in seconds
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=300))]
    pub timeout: u64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct HealthSuccessOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: HealthData,
    meta: HealthMeta,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct HealthData {
    check: &'static str,
    alias: String,
    endpoint: String,
    path: String,
    healthy: bool,
    status_code: u16,
    latency_ms: u64,
    status: Option<String>,
    service: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct HealthMeta {
    server_version: Option<String>,
}

/// Execute a liveness probe.
pub async fn execute(args: PingArgs, output_config: OutputConfig) -> ExitCode {
    execute_probe(
        &args.alias,
        args.timeout,
        HealthProbe::Liveness,
        output_config,
    )
    .await
}

pub(super) async fn execute_probe(
    alias_name: &str,
    timeout_seconds: u64,
    probe: HealthProbe,
    output_config: OutputConfig,
) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let alias_lookup = alias_name.trim_end_matches('/');
    let alias = match AliasManager::new().and_then(|manager| manager.get(alias_lookup)) {
        Ok(alias) => alias,
        Err(error) => {
            return ops_output::emit_error(
                &formatter,
                HEALTH_FAMILY,
                "Failed to resolve health endpoint",
                &error,
                None,
                None,
                None,
            );
        }
    };
    let client = match AdminClient::new(&alias) {
        Ok(client) => client,
        Err(error) => {
            return ops_output::emit_error(
                &formatter,
                HEALTH_FAMILY,
                "Failed to create health client",
                &error,
                None,
                None,
                None,
            );
        }
    };
    execute_with_api(alias_name, timeout_seconds, probe, &client, &formatter).await
}

pub(super) async fn execute_with_api(
    alias_name: &str,
    timeout_seconds: u64,
    probe: HealthProbe,
    api: &dyn HealthApi,
    formatter: &Formatter,
) -> ExitCode {
    match api
        .check_health(probe, Duration::from_secs(timeout_seconds))
        .await
    {
        Ok(report) if report.healthy => {
            if formatter.is_json() {
                formatter.json(&success_output(alias_name, &report));
            } else {
                print_human(alias_name, &report, formatter);
            }
            ExitCode::Success
        }
        Ok(report) => ops_output::emit_message(
            formatter,
            HEALTH_FAMILY,
            ExitCode::NetworkError,
            format!(
                "{} probe for '{}' returned HTTP {} ({})",
                probe.as_str(),
                alias_name,
                report.status_code,
                report.status.as_deref().unwrap_or("unhealthy")
            ),
            Some("Retry after the service and its dependencies become healthy."),
        ),
        Err(error) => ops_output::emit_error(
            formatter,
            HEALTH_FAMILY,
            &format!("{} probe failed", probe.as_str()),
            &error,
            None,
            None,
            None,
        ),
    }
}

fn success_output(alias_name: &str, report: &HealthReport) -> HealthSuccessOutput {
    HealthSuccessOutput {
        schema_version: 3,
        output_type: HEALTH_FAMILY,
        status: "success",
        data: HealthData {
            check: report.probe.as_str(),
            alias: alias_name.trim_end_matches('/').to_string(),
            endpoint: report.endpoint.clone(),
            path: report.path.clone(),
            healthy: report.healthy,
            status_code: report.status_code,
            latency_ms: report.latency_ms,
            status: report.status.clone(),
            service: report.service.clone(),
        },
        meta: HealthMeta {
            server_version: report.server_version.clone(),
        },
    }
}

fn print_human(alias_name: &str, report: &HealthReport, formatter: &Formatter) {
    let state = match report.probe {
        HealthProbe::Liveness => "alive",
        HealthProbe::Readiness => "ready",
    };
    let identity = match (&report.service, &report.server_version) {
        (Some(service), Some(version)) => format!(
            "{} {}",
            formatter.sanitize_text(service),
            formatter.sanitize_text(version)
        ),
        (Some(service), None) => formatter.sanitize_text(service),
        (None, Some(version)) => formatter.sanitize_text(version),
        (None, None) => "unknown service".to_string(),
    };
    formatter.println(&format!(
        "{} {}{}: {} in {} ms ({identity})",
        formatter.style_name(alias_name.trim_end_matches('/')),
        formatter.style_url(&report.endpoint),
        report.path,
        state,
        report.latency_ms
    ));
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use rc_core::Error;

    use super::*;

    struct MockHealthApi(Result<HealthReport, Error>);

    #[async_trait]
    impl HealthApi for MockHealthApi {
        async fn check_health(
            &self,
            _probe: HealthProbe,
            _timeout: Duration,
        ) -> Result<HealthReport, Error> {
            match &self.0 {
                Ok(report) => Ok(report.clone()),
                Err(Error::Network(message)) => Err(Error::Network(message.clone())),
                Err(error) => Err(Error::General(error.to_string())),
            }
        }
    }

    fn report(probe: HealthProbe, healthy: bool, status_code: u16) -> HealthReport {
        HealthReport {
            probe,
            endpoint: "http://127.0.0.1:9000".to_string(),
            path: probe.path().to_string(),
            status_code,
            healthy,
            latency_ms: 7,
            status: Some(if healthy { "ok" } else { "degraded" }.to_string()),
            service: Some("rustfs-endpoint".to_string()),
            server_version: Some("1.0.0-beta.10".to_string()),
        }
    }

    #[tokio::test]
    async fn ping_returns_success_for_healthy_liveness() {
        let api = MockHealthApi(Ok(report(HealthProbe::Liveness, true, 200)));
        let formatter = Formatter::default();

        assert_eq!(
            execute_with_api("local", 5, HealthProbe::Liveness, &api, &formatter).await,
            ExitCode::Success
        );
    }

    #[tokio::test]
    async fn ping_maps_timeout_to_network_exit() {
        let api = MockHealthApi(Err(Error::Network(
            "liveness probe timed out after 5 ms".to_string(),
        )));
        let formatter = Formatter::default();

        assert_eq!(
            execute_with_api("local", 5, HealthProbe::Liveness, &api, &formatter).await,
            ExitCode::NetworkError
        );
    }

    #[test]
    fn health_json_uses_additive_v3_contract() {
        let output = success_output("local/", &report(HealthProbe::Liveness, true, 200));
        let value = serde_json::to_value(output).expect("health output should serialize");

        assert_eq!(value["schema_version"], 3);
        assert_eq!(value["type"], "health");
        assert_eq!(value["data"]["check"], "liveness");
        assert_eq!(value["data"]["alias"], "local");
    }

    #[test]
    fn human_output_sanitizes_server_identity() {
        let formatter = Formatter::new(OutputConfig {
            no_color: true,
            ..Default::default()
        });
        let mut report = report(HealthProbe::Liveness, true, 200);
        report.service = Some("rustfs\n\u{1b}[31m".to_string());

        let identity = formatter.sanitize_text(report.service.as_deref().expect("service"));
        assert_eq!(identity, "rustfs\\n\\u{1b}[31m");
    }
}
