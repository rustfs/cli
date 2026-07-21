//! Bounded realtime metrics query and normalization.

use std::collections::BTreeMap;

use clap::{Args, ValueEnum};
use rc_core::admin::{
    MAX_METRICS_SAMPLES, MetricGroup, MetricsQuery, MetricsScope, ObservabilityApi, RealtimeMetrics,
};
use serde::Serialize;
use serde_json::{Number, Value};

use super::{emit_observability_error, get_admin_client};
use crate::exit_code::ExitCode;
use crate::output::Formatter;

const UNKNOWN_COLLECTED_AT: &str = "1970-01-01T00:00:00Z";

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum MetricsFormat {
    #[default]
    Normalized,
    Raw,
}

#[derive(Args, Debug)]
pub struct MetricsArgs {
    /// Alias name of the server
    pub alias: String,

    /// Comma-separated metric scopes: scanner,disk,os,batch-jobs,site-resync,network,memory,cpu,rpc,all
    #[arg(long, value_delimiter = ',', value_parser = parse_scope, default_value = "all")]
    pub scope: Vec<MetricsScope>,

    /// Number of snapshots to request
    #[arg(long, default_value_t = 1)]
    pub samples: u16,

    /// Interval between snapshots, such as 3s
    #[arg(long)]
    pub interval: Option<String>,

    /// Limit metrics to a host; may be repeated
    #[arg(long = "host")]
    pub hosts: Vec<String>,

    /// Limit metrics to a disk path; may be repeated
    #[arg(long = "disk")]
    pub disks: Vec<String>,

    /// Include metrics grouped by host
    #[arg(long)]
    pub by_host: bool,

    /// Include metrics grouped by disk
    #[arg(long)]
    pub by_disk: bool,

    /// Limit batch metrics to a job ID
    #[arg(long)]
    pub job_id: Option<String>,

    /// Limit site metrics to a deployment ID
    #[arg(long)]
    pub deployment_id: Option<String>,

    /// Select normalized v3 JSON Lines or bounded raw server records
    #[arg(long, value_enum, default_value_t)]
    pub metrics_format: MetricsFormat,
}

#[derive(Debug, Serialize)]
struct MetricsSuccessOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: MetricsData,
}

#[derive(Debug, Serialize)]
struct MetricsData {
    scope: String,
    collected_at: String,
    samples: Vec<MetricSample>,
    errors: Vec<String>,
    partial: bool,
    #[serde(rename = "final")]
    final_sample: bool,
    raw: Value,
}

#[derive(Debug, Serialize)]
struct MetricSample {
    name: String,
    value: Number,
    unit: Option<String>,
    labels: BTreeMap<String, String>,
    collected_at: String,
}

pub async fn execute(args: MetricsArgs, formatter: &Formatter) -> ExitCode {
    if args.samples == 0 || args.samples > MAX_METRICS_SAMPLES {
        return formatter.fail(
            ExitCode::UsageError,
            &format!("Metrics samples must be between 1 and {MAX_METRICS_SAMPLES}"),
        );
    }

    let query = MetricsQuery {
        scopes: args.scope,
        hosts: args.hosts,
        disks: args.disks,
        interval: args.interval,
        samples: args.samples,
        by_host: args.by_host,
        by_disk: args.by_disk,
        job_id: args.job_id,
        deployment_id: args.deployment_id,
    };
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.realtime_metrics(&query).await {
        Ok(batch) => {
            if batch.snapshots.is_empty() {
                formatter.println("No metric snapshots returned");
                return ExitCode::Success;
            }
            for snapshot in &batch.snapshots {
                let result = match args.metrics_format {
                    MetricsFormat::Raw => serde_json::to_string(snapshot),
                    MetricsFormat::Normalized => {
                        serde_json::to_string(&normalize_snapshot(snapshot, &query))
                    }
                };
                match result {
                    Ok(record) => formatter.println(&record),
                    Err(error) => {
                        return formatter.fail(
                            ExitCode::GeneralError,
                            &format!("Failed to serialize metrics output: {error}"),
                        );
                    }
                }
            }
            ExitCode::Success
        }
        Err(error) => emit_observability_error(
            "metrics",
            "admin.metrics",
            "Failed to query realtime metrics",
            &error,
            formatter,
        ),
    }
}

fn normalize_snapshot(snapshot: &RealtimeMetrics, query: &MetricsQuery) -> MetricsSuccessOutput {
    let mut samples = Vec::new();
    let mut latest = None;

    for (scope, group) in snapshot.aggregated.groups() {
        collect_group(scope, group, BTreeMap::new(), &mut samples, &mut latest);
    }
    for (host, groups) in &snapshot.by_host {
        for (scope, group) in groups.groups() {
            collect_group(
                scope,
                group,
                BTreeMap::from([("host".to_string(), host.clone())]),
                &mut samples,
                &mut latest,
            );
        }
    }
    for (disk, group) in &snapshot.by_disk {
        collect_group(
            "disk",
            group,
            BTreeMap::from([("disk".to_string(), disk.clone())]),
            &mut samples,
            &mut latest,
        );
    }

    MetricsSuccessOutput {
        schema_version: 3,
        output_type: "metrics",
        status: "success",
        data: MetricsData {
            scope: query.scope_label(),
            collected_at: latest.unwrap_or_else(|| UNKNOWN_COLLECTED_AT.to_string()),
            samples,
            errors: snapshot.errors.clone(),
            partial: !snapshot.errors.is_empty() || !snapshot.final_sample,
            final_sample: snapshot.final_sample,
            raw: serde_json::to_value(snapshot).unwrap_or(Value::Null),
        },
    }
}

fn collect_group(
    scope: &str,
    group: &MetricGroup,
    labels: BTreeMap<String, String>,
    samples: &mut Vec<MetricSample>,
    latest: &mut Option<String>,
) {
    let object = group
        .0
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    collect_object(
        &object,
        &[scope.to_string()],
        &labels,
        None,
        samples,
        latest,
    );
}

fn collect_object(
    object: &serde_json::Map<String, Value>,
    path: &[String],
    labels: &BTreeMap<String, String>,
    inherited_timestamp: Option<&str>,
    samples: &mut Vec<MetricSample>,
    latest: &mut Option<String>,
) {
    let timestamp = object
        .iter()
        .find_map(|(key, value)| is_timestamp_key(key).then(|| value.as_str()).flatten())
        .or(inherited_timestamp);
    if let Some(timestamp) = timestamp {
        update_latest(latest, timestamp);
    }

    for (key, value) in object {
        if is_timestamp_key(key) {
            continue;
        }
        let mut child_path = path.to_vec();
        child_path.push(to_snake_case(key));
        match value {
            Value::Number(number) => {
                let mut sample_labels = labels.clone();
                if path.last().is_some_and(|segment| {
                    matches!(
                        segment.as_str(),
                        "life_time_ops" | "last_minute" | "api_calls"
                    )
                }) {
                    sample_labels.insert("operation".to_string(), key.clone());
                }
                samples.push(MetricSample {
                    name: child_path.join("_"),
                    value: number.clone(),
                    unit: None,
                    labels: sample_labels,
                    collected_at: timestamp.unwrap_or(UNKNOWN_COLLECTED_AT).to_string(),
                });
            }
            Value::Object(child) => {
                collect_object(child, &child_path, labels, timestamp, samples, latest)
            }
            _ => {}
        }
    }
}

fn update_latest(latest: &mut Option<String>, candidate: &str) {
    if latest.as_deref().is_none_or(|current| candidate > current) {
        *latest = Some(candidate.to_string());
    }
}

fn is_timestamp_key(key: &str) -> bool {
    matches!(key, "collected" | "collectedAt" | "collected_at")
}

fn to_snake_case(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for (index, character) in value.chars().enumerate() {
        if character == '-' || character == ' ' {
            output.push('_');
        } else if character.is_ascii_uppercase() {
            if index > 0 && !output.ends_with('_') {
                output.push('_');
            }
            output.push(character.to_ascii_lowercase());
        } else {
            output.push(character);
        }
    }
    output
}

fn parse_scope(value: &str) -> Result<MetricsScope, String> {
    match value {
        "scanner" => Ok(MetricsScope::Scanner),
        "disk" => Ok(MetricsScope::Disk),
        "os" => Ok(MetricsScope::Os),
        "batch-jobs" | "batch" => Ok(MetricsScope::BatchJobs),
        "site-resync" => Ok(MetricsScope::SiteResync),
        "network" | "net" => Ok(MetricsScope::Network),
        "memory" | "mem" => Ok(MetricsScope::Memory),
        "cpu" => Ok(MetricsScope::Cpu),
        "rpc" => Ok(MetricsScope::Rpc),
        "all" => Ok(MetricsScope::All),
        _ => Err(format!("Unknown metrics scope '{value}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_normalizes_server_metric_names() {
        assert_eq!(to_snake_case("incomingBytes"), "incoming_bytes");
        assert_eq!(to_snake_case("site-resync"), "site_resync");
    }
}
