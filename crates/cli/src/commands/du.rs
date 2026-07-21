//! Storage usage command with RustFS snapshot and explicit S3 fallback.

use clap::Args;
use jiff::Timestamp;
use rc_core::admin::{CapabilityApi, CapabilityAvailability, CapabilityReport};
use rc_core::ops::{UsageBucket, UsageReport, UsageScanApi, UsageScanRequest, UsageSnapshotApi};
use rc_core::{AliasManager, Error};
use rc_s3::{AdminClient, S3Client};
use serde::Serialize;

use super::ops_output;
use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

const USAGE_FAMILY: &str = "usage";
const DATA_USAGE_CAPABILITY: &str = "admin.data-usage";
const SNAPSHOT_STALE_AFTER_SECONDS: u64 = 3_600;
const FALLBACK_SUGGESTION: &str = "Retry with --fallback to permit a slower client-side S3 scan.";

/// Summarize storage usage from a RustFS snapshot or explicit S3 scan.
#[derive(Args, Debug)]
pub struct DuArgs {
    /// Alias, bucket, or prefix path (ALIAS[/BUCKET[/PREFIX]])
    pub target: String,

    /// Permit a slower client-side S3 scan when the server snapshot cannot be used
    #[arg(long)]
    pub fallback: bool,

    /// Include all object versions and delete markers during a client scan
    #[arg(long)]
    pub versions: bool,

    /// Include incomplete multipart uploads and uploaded part bytes during a client scan
    #[arg(long)]
    pub incomplete: bool,
}

#[derive(Debug)]
struct ParsedUsageTarget {
    alias: String,
    request: UsageScanRequest,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct UsageSuccessOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: UsageData,
    meta: UsageMeta,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct UsageData {
    scope: &'static str,
    path: Option<String>,
    source: &'static str,
    snapshot_at: Option<String>,
    snapshot_age_seconds: Option<u64>,
    stale: bool,
    total_bytes: u64,
    object_count: u64,
    version_count: Option<u64>,
    delete_marker_count: Option<u64>,
    incomplete_upload_count: Option<u64>,
    incomplete_upload_bytes: Option<u64>,
    buckets: Vec<UsageBucketOutput>,
    partial: bool,
    failures: Vec<UsageFailureOutput>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct UsageBucketOutput {
    name: String,
    total_bytes: u64,
    object_count: u64,
    version_count: Option<u64>,
    delete_marker_count: Option<u64>,
    incomplete_upload_count: Option<u64>,
    incomplete_upload_bytes: Option<u64>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct UsageFailureOutput {
    bucket: String,
    message: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct UsageMeta {
    server_version: Option<String>,
}

/// Execute a usage query.
pub async fn execute(args: DuArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let mut target = match parse_target(&args.target) {
        Ok(target) => target,
        Err(error) => {
            return ops_output::emit_error(
                &formatter,
                USAGE_FAMILY,
                "Invalid usage target",
                &error,
                None,
                None,
                None,
            );
        }
    };
    apply_scan_flags(&mut target, &args);
    let alias = match AliasManager::new().and_then(|manager| manager.get(&target.alias)) {
        Ok(alias) => alias,
        Err(error) => {
            return ops_output::emit_error(
                &formatter,
                USAGE_FAMILY,
                "Failed to resolve usage endpoint",
                &error,
                None,
                None,
                None,
            );
        }
    };
    let admin = match AdminClient::new(&alias) {
        Ok(client) => client,
        Err(error) => {
            return ops_output::emit_error(
                &formatter,
                USAGE_FAMILY,
                "Failed to create usage snapshot client",
                &error,
                None,
                None,
                None,
            );
        }
    };
    let scan_client = if args.fallback {
        match S3Client::new(alias).await {
            Ok(client) => Some(client),
            Err(error) => {
                return ops_output::emit_error(
                    &formatter,
                    USAGE_FAMILY,
                    "Failed to create usage scan client",
                    &error,
                    None,
                    None,
                    None,
                );
            }
        }
    } else {
        None
    };

    execute_with_apis(
        &args,
        &target,
        &admin,
        &admin,
        scan_client
            .as_ref()
            .map(|client| client as &dyn UsageScanApi),
        Timestamp::now(),
        &formatter,
    )
    .await
}

async fn execute_with_apis(
    args: &DuArgs,
    target: &ParsedUsageTarget,
    capabilities: &dyn CapabilityApi,
    snapshot_api: &dyn UsageSnapshotApi,
    scan_api: Option<&dyn UsageScanApi>,
    now: Timestamp,
    formatter: &Formatter,
) -> ExitCode {
    if target.request.requires_client_scan() {
        if !args.fallback {
            let error = Error::UnsupportedFeature(
                "The RustFS data-usage snapshot does not provide exact prefix or incomplete-upload usage"
                    .to_string(),
            );
            return ops_output::emit_error(
                formatter,
                USAGE_FAMILY,
                "Server usage snapshot cannot satisfy the requested dimensions",
                &error,
                Some(DATA_USAGE_CAPABILITY),
                None,
                Some(FALLBACK_SUGGESTION),
            );
        }
        return execute_scan(scan_api, target, None, now, formatter).await;
    }

    let capability_report = capabilities.discover_capabilities(false).await;
    let server_version = capability_report
        .as_ref()
        .ok()
        .and_then(|report| report.server_version.clone());

    let fast_path = match capability_report {
        Ok(report) if data_usage_available(&report) => snapshot_api
            .usage_snapshot()
            .await
            .and_then(|report| scope_snapshot(report, &target.alias, &target.request)),
        Ok(report) => Err(Error::UnsupportedFeature(capability_reason(&report))),
        Err(error) => Err(error),
    };

    match fast_path {
        Ok(report) => emit_report(report, server_version, now, formatter),
        Err(_error) if args.fallback => {
            execute_scan(scan_api, target, server_version, now, formatter).await
        }
        Err(error) => ops_output::emit_error(
            formatter,
            USAGE_FAMILY,
            "Server usage snapshot is unavailable",
            &error,
            Some(DATA_USAGE_CAPABILITY),
            server_version.as_deref(),
            Some(FALLBACK_SUGGESTION),
        ),
    }
}

async fn execute_scan(
    scan_api: Option<&dyn UsageScanApi>,
    target: &ParsedUsageTarget,
    server_version: Option<String>,
    now: Timestamp,
    formatter: &Formatter,
) -> ExitCode {
    let Some(scan_api) = scan_api else {
        return ops_output::emit_message(
            formatter,
            USAGE_FAMILY,
            ExitCode::GeneralError,
            "Client-side usage fallback was permitted but no scan adapter is available",
            None,
        );
    };
    match scan_api.scan_usage(&target.request).await {
        Ok(mut report) => {
            report.path = target.request.path(&target.alias);
            emit_report(report, server_version, now, formatter)
        }
        Err(scan_error) => ops_output::emit_error(
            formatter,
            USAGE_FAMILY,
            "Client-side usage scan failed",
            &scan_error,
            None,
            server_version.as_deref(),
            None,
        ),
    }
}

fn emit_report(
    report: UsageReport,
    server_version: Option<String>,
    now: Timestamp,
    formatter: &Formatter,
) -> ExitCode {
    if formatter.is_json() {
        formatter.json(&success_output(&report, server_version, now));
    } else {
        print_human(&report, now, formatter);
    }

    if report.partial {
        if !formatter.is_json() {
            formatter.warning("Usage scan is partial; one or more buckets could not be read.");
        }
        ExitCode::NetworkError
    } else {
        ExitCode::Success
    }
}

fn data_usage_available(report: &CapabilityReport) -> bool {
    report.capabilities.iter().any(|capability| {
        capability.name == DATA_USAGE_CAPABILITY
            && capability.availability == CapabilityAvailability::Available
    })
}

fn capability_reason(report: &CapabilityReport) -> String {
    report
        .capabilities
        .iter()
        .find(|capability| capability.name == DATA_USAGE_CAPABILITY)
        .and_then(|capability| capability.reason.clone())
        .unwrap_or_else(|| {
            "Capability discovery did not advertise the RustFS data-usage route".to_string()
        })
}

fn scope_snapshot(
    mut report: UsageReport,
    alias: &str,
    request: &UsageScanRequest,
) -> Result<UsageReport, Error> {
    report.scope = request.scope();
    report.path = request.path(alias);
    let Some(bucket_name) = request.bucket.as_deref() else {
        return Ok(report);
    };

    let bucket = report
        .buckets
        .iter()
        .find(|bucket| bucket.name == bucket_name)
        .cloned()
        .ok_or_else(|| {
            Error::NotFound(format!(
                "Bucket '{alias}/{bucket_name}' is absent from the server usage snapshot"
            ))
        })?;
    report.buckets = vec![bucket];
    report.finish();
    Ok(report)
}

fn success_output(
    report: &UsageReport,
    server_version: Option<String>,
    now: Timestamp,
) -> UsageSuccessOutput {
    let snapshot_age_seconds = report
        .snapshot_at
        .map(|snapshot| age_seconds(now, snapshot));
    UsageSuccessOutput {
        schema_version: 3,
        output_type: USAGE_FAMILY,
        status: "success",
        data: UsageData {
            scope: report.scope.as_str(),
            path: report.path.clone(),
            source: report.source.as_str(),
            snapshot_at: report.snapshot_at.map(|timestamp| timestamp.to_string()),
            snapshot_age_seconds,
            stale: snapshot_age_seconds.is_some_and(|age| age > SNAPSHOT_STALE_AFTER_SECONDS),
            total_bytes: report.total_bytes,
            object_count: report.object_count,
            version_count: report.version_count,
            delete_marker_count: report.delete_marker_count,
            incomplete_upload_count: report.incomplete_upload_count,
            incomplete_upload_bytes: report.incomplete_upload_bytes,
            buckets: report.buckets.iter().map(bucket_output).collect(),
            partial: report.partial,
            failures: report
                .failures
                .iter()
                .map(|failure| UsageFailureOutput {
                    bucket: failure.bucket.clone(),
                    message: failure.message.clone(),
                })
                .collect(),
        },
        meta: UsageMeta { server_version },
    }
}

fn bucket_output(bucket: &UsageBucket) -> UsageBucketOutput {
    UsageBucketOutput {
        name: bucket.name.clone(),
        total_bytes: bucket.total_bytes,
        object_count: bucket.object_count,
        version_count: bucket.version_count,
        delete_marker_count: bucket.delete_marker_count,
        incomplete_upload_count: bucket.incomplete_upload_count,
        incomplete_upload_bytes: bucket.incomplete_upload_bytes,
    }
}

fn age_seconds(now: Timestamp, snapshot: Timestamp) -> u64 {
    u64::try_from(now.as_second().saturating_sub(snapshot.as_second())).unwrap_or_default()
}

fn print_human(report: &UsageReport, now: Timestamp, formatter: &Formatter) {
    formatter.println(&format!("Source: {}", report.source.as_str()));
    formatter.println(&format!("Scope:  {}", report.scope.as_str()));
    if let Some(path) = &report.path {
        formatter.println(&format!("Path:   {}", formatter.sanitize_text(path)));
    }
    if let Some(snapshot) = report.snapshot_at {
        let age = age_seconds(now, snapshot);
        let stale = if age > SNAPSHOT_STALE_AFTER_SECONDS {
            ", stale"
        } else {
            ""
        };
        formatter.println(&format!(
            "Snapshot: {} ({} seconds old{stale})",
            snapshot, age
        ));
    }
    formatter.println("");
    formatter.println(
        "BUCKET                          BYTES       OBJECTS     VERSIONS    DELETE MARKERS",
    );
    for bucket in &report.buckets {
        formatter.println(&format!(
            "{:<31} {:>11} {:>11} {:>12} {:>17}",
            formatter.sanitize_text(&bucket.name),
            humansize::format_size(bucket.total_bytes, humansize::BINARY),
            bucket.object_count,
            optional_count(bucket.version_count),
            optional_count(bucket.delete_marker_count),
        ));
    }
    formatter.println(&format!(
        "Total: {}, {} object(s), {} version(s), {} delete marker(s)",
        humansize::format_size(report.total_bytes, humansize::BINARY),
        report.object_count,
        optional_count(report.version_count),
        optional_count(report.delete_marker_count),
    ));
    if let Some(uploads) = report.incomplete_upload_count {
        formatter.println(&format!(
            "Incomplete: {} upload(s), {}",
            uploads,
            humansize::format_size(
                report.incomplete_upload_bytes.unwrap_or_default(),
                humansize::BINARY
            )
        ));
    }
    for failure in &report.failures {
        formatter.warning(&format!(
            "Bucket '{}': {}",
            formatter.sanitize_text(&failure.bucket),
            formatter.sanitize_text(&failure.message)
        ));
    }
}

fn optional_count(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), |value| value.to_string())
}

fn parse_target(value: &str) -> Result<ParsedUsageTarget, Error> {
    if value.is_empty() {
        return Err(Error::InvalidPath(
            "Usage target cannot be empty".to_string(),
        ));
    }
    let parts = value.splitn(3, '/').collect::<Vec<_>>();
    let alias = parts[0];
    if alias.is_empty() {
        return Err(Error::InvalidPath(
            "Usage target must begin with an alias".to_string(),
        ));
    }
    let bucket = parts
        .get(1)
        .copied()
        .filter(|component| !component.is_empty())
        .map(str::to_string);
    let prefix = parts
        .get(2)
        .copied()
        .filter(|component| !component.is_empty())
        .map(str::to_string);
    if parts.get(2).is_some() && bucket.is_none() {
        return Err(Error::InvalidPath(
            "A usage prefix requires a non-empty bucket name".to_string(),
        ));
    }

    Ok(ParsedUsageTarget {
        alias: alias.to_string(),
        request: UsageScanRequest {
            bucket,
            prefix,
            include_versions: false,
            include_incomplete_uploads: false,
        },
    })
}

fn apply_scan_flags(target: &mut ParsedUsageTarget, args: &DuArgs) {
    target.request.include_versions = args.versions;
    target.request.include_incomplete_uploads = args.incomplete;
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use rc_core::admin::{CapabilityEntry, ClusterSnapshotMetadata, ExtensionMetadata};
    use rc_core::ops::{UsageFailure, UsageScope, UsageSource};

    use super::*;

    struct MockCapabilities(Result<CapabilityReport, Error>);

    #[async_trait]
    impl CapabilityApi for MockCapabilities {
        async fn discover_capabilities(&self, _refresh: bool) -> Result<CapabilityReport, Error> {
            match &self.0 {
                Ok(report) => Ok(report.clone()),
                Err(Error::Auth(message)) => Err(Error::Auth(message.clone())),
                Err(error) => Err(Error::General(error.to_string())),
            }
        }
    }

    struct MockSnapshot(Result<UsageReport, Error>);

    #[async_trait]
    impl UsageSnapshotApi for MockSnapshot {
        async fn usage_snapshot(&self) -> Result<UsageReport, Error> {
            match &self.0 {
                Ok(report) => Ok(report.clone()),
                Err(Error::Auth(message)) => Err(Error::Auth(message.clone())),
                Err(error) => Err(Error::General(error.to_string())),
            }
        }
    }

    struct MockScan {
        calls: AtomicUsize,
        report: UsageReport,
    }

    #[async_trait]
    impl UsageScanApi for MockScan {
        async fn scan_usage(&self, _request: &UsageScanRequest) -> Result<UsageReport, Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.report.clone())
        }
    }

    fn capability_report(available: bool) -> CapabilityReport {
        CapabilityReport {
            server_version: Some("1.0.0-beta.10".to_string()),
            runtime_path: "/runtime".to_string(),
            extensions_path: "/extensions".to_string(),
            cluster_snapshot_path: "/snapshot".to_string(),
            capabilities: vec![CapabilityEntry {
                name: DATA_USAGE_CAPABILITY.to_string(),
                availability: if available {
                    CapabilityAvailability::Available
                } else {
                    CapabilityAvailability::VersionGated
                },
                reason: (!available).then(|| "route unavailable".to_string()),
            }],
            extensions: Vec::<ExtensionMetadata>::new(),
            cluster: ClusterSnapshotMetadata {
                summary: None,
                runtime_capabilities_path: None,
                extensions_catalog_path: None,
            },
        }
    }

    fn snapshot(timestamp: i64) -> UsageReport {
        UsageReport {
            source: UsageSource::ServerSnapshot,
            scope: UsageScope::Cluster,
            path: None,
            snapshot_at: Timestamp::from_second(timestamp).ok(),
            total_bytes: 10,
            object_count: 1,
            version_count: Some(2),
            delete_marker_count: Some(1),
            incomplete_upload_count: None,
            incomplete_upload_bytes: None,
            buckets: vec![UsageBucket {
                name: "photos".to_string(),
                total_bytes: 10,
                object_count: 1,
                version_count: Some(2),
                delete_marker_count: Some(1),
                incomplete_upload_count: None,
                incomplete_upload_bytes: None,
            }],
            partial: false,
            failures: Vec::new(),
        }
    }

    fn args(target: &str, fallback: bool) -> DuArgs {
        DuArgs {
            target: target.to_string(),
            fallback,
            versions: false,
            incomplete: false,
        }
    }

    fn parsed(args: &DuArgs) -> ParsedUsageTarget {
        let mut target = parse_target(&args.target).expect("test target should parse");
        apply_scan_flags(&mut target, args);
        target
    }

    #[tokio::test]
    async fn du_uses_advertised_server_snapshot_without_scanning() {
        let command = args("local", false);
        let target = parsed(&command);
        let scan = MockScan {
            calls: AtomicUsize::new(0),
            report: UsageReport::empty(UsageSource::ClientScan, UsageScope::Cluster, None),
        };
        let code = execute_with_apis(
            &command,
            &target,
            &MockCapabilities(Ok(capability_report(true))),
            &MockSnapshot(Ok(snapshot(1_700_000_000))),
            Some(&scan),
            Timestamp::from_second(1_700_000_100).expect("timestamp"),
            &Formatter::default(),
        )
        .await;

        assert_eq!(code, ExitCode::Success);
        assert_eq!(scan.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn auth_denial_never_scans_without_explicit_fallback() {
        let command = args("local", false);
        let target = parsed(&command);
        let scan = MockScan {
            calls: AtomicUsize::new(0),
            report: UsageReport::empty(UsageSource::ClientScan, UsageScope::Cluster, None),
        };
        let code = execute_with_apis(
            &command,
            &target,
            &MockCapabilities(Err(Error::Auth("denied".to_string()))),
            &MockSnapshot(Ok(snapshot(1_700_000_000))),
            Some(&scan),
            Timestamp::from_second(1_700_000_100).expect("timestamp"),
            &Formatter::default(),
        )
        .await;

        assert_eq!(code, ExitCode::AuthError);
        assert_eq!(scan.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn explicit_fallback_scans_when_capability_is_unavailable() {
        let command = args("local/photos", true);
        let target = parsed(&command);
        let scan = MockScan {
            calls: AtomicUsize::new(0),
            report: UsageReport::empty(UsageSource::ClientScan, UsageScope::Bucket, None),
        };
        let code = execute_with_apis(
            &command,
            &target,
            &MockCapabilities(Ok(capability_report(false))),
            &MockSnapshot(Ok(snapshot(1_700_000_000))),
            Some(&scan),
            Timestamp::from_second(1_700_000_100).expect("timestamp"),
            &Formatter::default(),
        )
        .await;

        assert_eq!(code, ExitCode::Success);
        assert_eq!(scan.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn partial_fallback_returns_network_exit_with_partial_result() {
        let command = args("local", true);
        let target = parsed(&command);
        let mut partial = UsageReport::empty(UsageSource::ClientScan, UsageScope::Cluster, None);
        partial.push_failure(UsageFailure {
            bucket: "private".to_string(),
            message: "Access denied".to_string(),
        });
        let scan = MockScan {
            calls: AtomicUsize::new(0),
            report: partial,
        };
        let code = execute_with_apis(
            &command,
            &target,
            &MockCapabilities(Ok(capability_report(false))),
            &MockSnapshot(Ok(snapshot(1_700_000_000))),
            Some(&scan),
            Timestamp::from_second(1_700_000_100).expect("timestamp"),
            &Formatter::default(),
        )
        .await;

        assert_eq!(code, ExitCode::NetworkError);
        assert_eq!(scan.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stale_snapshot_exposes_timestamp_age_and_source() {
        let output = success_output(
            &snapshot(1_700_000_000),
            Some("1.0.0-beta.10".to_string()),
            Timestamp::from_second(1_700_007_201).expect("timestamp"),
        );
        let value = serde_json::to_value(output).expect("usage output should serialize");

        assert_eq!(value["data"]["source"], "server_snapshot");
        assert_eq!(value["data"]["snapshot_age_seconds"], 7_201);
        assert_eq!(value["data"]["stale"], true);
        assert!(value["data"]["snapshot_at"].as_str().is_some());
    }

    #[test]
    fn target_parser_distinguishes_cluster_bucket_and_prefix() {
        assert_eq!(
            parse_target("local").expect("cluster").request.scope(),
            UsageScope::Cluster
        );
        assert_eq!(
            parse_target("local/photos")
                .expect("bucket")
                .request
                .scope(),
            UsageScope::Bucket
        );
        assert_eq!(
            parse_target("local/photos/2026/")
                .expect("prefix")
                .request
                .scope(),
            UsageScope::Prefix
        );
    }
}
