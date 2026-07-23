//! Public RustFS readiness probe command.

use clap::Args;

use super::ping;
use crate::exit_code::ExitCode;
use crate::output::OutputConfig;

/// Check whether the service and required dependencies are ready.
#[derive(Args, Debug)]
pub struct ReadyArgs {
    /// Alias name of the service endpoint
    pub alias: String,

    /// Maximum probe duration in seconds
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=300))]
    pub timeout: u64,
}

/// Execute a readiness probe.
pub async fn execute(args: ReadyArgs, output_config: OutputConfig) -> ExitCode {
    ping::execute_probe(
        &args.alias,
        args.timeout,
        rc_core::ops::HealthProbe::Readiness,
        output_config,
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;
    use rc_core::ops::{HealthApi, HealthProbe, HealthReport};
    use rc_core::{Error, Result};

    use super::*;
    use crate::output::Formatter;

    struct ReadyApi {
        report: Option<HealthReport>,
        error: Option<String>,
    }

    #[async_trait]
    impl HealthApi for ReadyApi {
        async fn check_health(
            &self,
            _probe: HealthProbe,
            _timeout: Duration,
        ) -> Result<HealthReport> {
            if let Some(error) = &self.error {
                return Err(Error::Network(error.clone()));
            }
            self.report
                .clone()
                .ok_or_else(|| Error::General("missing test report".to_string()))
        }
    }

    fn ready_report(healthy: bool, status_code: u16) -> HealthReport {
        HealthReport {
            probe: HealthProbe::Readiness,
            endpoint: "http://127.0.0.1:9000".to_string(),
            path: "/health/ready".to_string(),
            status_code,
            healthy,
            latency_ms: 4,
            status: Some(if healthy { "ok" } else { "degraded" }.to_string()),
            service: Some("rustfs-endpoint".to_string()),
            server_version: Some("1.0.0-beta.10".to_string()),
        }
    }

    #[tokio::test]
    async fn ready_returns_success_when_dependencies_are_ready() {
        let api = ReadyApi {
            report: Some(ready_report(true, 200)),
            error: None,
        };
        assert_eq!(
            ping::execute_with_api(
                "local",
                5,
                HealthProbe::Readiness,
                &api,
                &Formatter::default(),
            )
            .await,
            ExitCode::Success
        );
    }

    #[tokio::test]
    async fn ready_maps_service_unavailable_to_network_exit() {
        let api = ReadyApi {
            report: Some(ready_report(false, 503)),
            error: None,
        };
        assert_eq!(
            ping::execute_with_api(
                "local",
                5,
                HealthProbe::Readiness,
                &api,
                &Formatter::default(),
            )
            .await,
            ExitCode::NetworkError
        );
    }

    #[tokio::test]
    async fn ready_maps_timeout_to_network_exit() {
        let api = ReadyApi {
            report: None,
            error: Some("readiness probe timed out".to_string()),
        };
        assert_eq!(
            ping::execute_with_api(
                "local",
                5,
                HealthProbe::Readiness,
                &api,
                &Formatter::default(),
            )
            .await,
            ExitCode::NetworkError
        );
    }
}
