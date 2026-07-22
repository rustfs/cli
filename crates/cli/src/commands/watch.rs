//! Live RustFS object notification command.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::time::Duration;

use clap::Args;
use futures::StreamExt as _;
use rc_core::admin::{CapabilityApi as _, CapabilityAvailability, CapabilityReport};
use rc_core::{AliasManager, Error, WatchApi, WatchEvent, WatchFrame, WatchRequest};
use rc_s3::{AdminClient, S3Client};
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

const WATCH_AFTER_HELP: &str = "\
Examples:
  rc watch local/
  rc watch local/photos --event put --event delete --prefix incoming/
  rc watch local/photos --event put,delete,get --suffix .json --json";

const DEFAULT_RECONNECT_ATTEMPTS: u32 = 5;
const DEFAULT_RECONNECT_DELAY_MS: u64 = 500;
const DEFAULT_RECONNECT_MAX_DELAY_MS: u64 = 10_000;
const DEFAULT_PING_SECONDS: u64 = 10;

/// Stream object events from a RustFS service or bucket.
#[derive(Args, Debug, Clone)]
#[command(after_help = WATCH_AFTER_HELP)]
pub struct WatchArgs {
    /// Alias or bucket scope (ALIAS[/BUCKET])
    pub path: String,

    /// Event filter (put, delete, get, or an S3 event name; repeatable and comma-separated)
    #[arg(short = 'e', long = "event", value_name = "EVENT")]
    pub events: Vec<String>,

    /// Match decoded object keys with this prefix
    #[arg(long)]
    pub prefix: Option<String>,

    /// Match decoded object keys with this suffix
    #[arg(long)]
    pub suffix: Option<String>,

    /// Server keepalive interval in seconds
    #[arg(long, default_value_t = DEFAULT_PING_SECONDS)]
    pub ping: u64,

    /// Maximum reconnects after the initial connection
    #[arg(long, default_value_t = DEFAULT_RECONNECT_ATTEMPTS)]
    pub reconnect_attempts: u32,

    /// Initial reconnect delay in milliseconds
    #[arg(long, default_value_t = DEFAULT_RECONNECT_DELAY_MS)]
    pub reconnect_delay_ms: u64,

    /// Maximum reconnect delay in milliseconds
    #[arg(long, default_value_t = DEFAULT_RECONNECT_MAX_DELAY_MS)]
    pub reconnect_max_delay_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchTermination {
    Cancelled,
    OutputClosed,
}

#[derive(Debug, Clone, Copy)]
struct ReconnectPolicy {
    max_reconnects: u32,
    initial_delay: Duration,
    max_delay: Duration,
}

impl ReconnectPolicy {
    fn from_args(args: &WatchArgs) -> Result<Self, String> {
        if args.ping == 0 {
            return Err("--ping must be greater than zero".to_string());
        }
        if args.reconnect_delay_ms == 0 {
            return Err("--reconnect-delay-ms must be greater than zero".to_string());
        }
        if args.reconnect_max_delay_ms < args.reconnect_delay_ms {
            return Err(
                "--reconnect-max-delay-ms must be greater than or equal to --reconnect-delay-ms"
                    .to_string(),
            );
        }

        Ok(Self {
            max_reconnects: args.reconnect_attempts,
            initial_delay: Duration::from_millis(args.reconnect_delay_ms),
            max_delay: Duration::from_millis(args.reconnect_max_delay_ms),
        })
    }

    fn delay_for(self, reconnect: u32) -> Duration {
        let shift = reconnect.saturating_sub(1).min(31);
        let multiplier = 1_u32.checked_shl(shift).unwrap_or(u32::MAX);
        self.initial_delay
            .saturating_mul(multiplier)
            .min(self.max_delay)
    }
}

#[derive(Debug, Serialize)]
struct WatchSuccessOutput<'a> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: WatchSuccessData<'a>,
}

#[derive(Debug, Serialize)]
struct WatchSuccessData<'a> {
    event: &'a WatchEvent,
    keepalive: bool,
}

#[derive(Debug, Serialize)]
struct WatchErrorOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    error: WatchErrorBody,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WatchErrorBody {
    Unsupported(WatchUnsupportedError),
    Standard(WatchStandardError),
}

#[derive(Debug, Serialize)]
struct WatchUnsupportedError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    capability: &'static str,
    server: Option<String>,
    suggestion: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct WatchStandardError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    suggestion: Option<&'static str>,
}

/// Execute the watch command.
pub async fn execute(args: WatchArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let (alias_name, request) = match build_watch_request(&args) {
        Ok(request) => request,
        Err(message) => {
            return fail_watch(&formatter, ExitCode::UsageError, &message, None);
        }
    };
    let policy = match ReconnectPolicy::from_args(&args) {
        Ok(policy) => policy,
        Err(message) => {
            return fail_watch(&formatter, ExitCode::UsageError, &message, None);
        }
    };

    let alias_manager = match AliasManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            return fail_watch(
                &formatter,
                ExitCode::GeneralError,
                &format!("Failed to load aliases: {error}"),
                None,
            );
        }
    };
    let alias = match alias_manager.get(&alias_name) {
        Ok(alias) => alias,
        Err(Error::AliasNotFound(_)) => {
            return fail_watch(
                &formatter,
                ExitCode::NotFound,
                &format!("Alias '{alias_name}' not found"),
                None,
            );
        }
        Err(error) => {
            return fail_watch(
                &formatter,
                exit_code_for_error(&error),
                &format!("Failed to load alias '{alias_name}': {error}"),
                None,
            );
        }
    };

    let mut cancellation: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(async {
        let _ = tokio::signal::ctrl_c().await;
    });

    let mut server_version = None;
    match AdminClient::new(&alias) {
        Ok(admin) => match tokio::select! {
            biased;
            _ = cancellation.as_mut() => return ExitCode::Interrupted,
            report = admin.discover_capabilities(false) => report,
        } {
            Ok(report) => {
                server_version = report.server_version.clone();
                if let Err((code, message)) = validate_watch_capability(&report) {
                    return fail_watch(&formatter, code, &message, server_version);
                }
            }
            Err(Error::Auth(message)) => {
                return fail_watch(&formatter, ExitCode::AuthError, &message, None);
            }
            Err(error) => formatter.warning(&format!(
                "Capability discovery was inconclusive; trying the watch route directly: {error}"
            )),
        },
        Err(error) => formatter.warning(&format!(
            "Capability discovery was unavailable; trying the watch route directly: {error}"
        )),
    }

    let client = match tokio::select! {
        biased;
        _ = cancellation.as_mut() => return ExitCode::Interrupted,
        client = S3Client::new(alias) => client,
    } {
        Ok(client) => client,
        Err(error) => {
            return fail_watch(
                &formatter,
                exit_code_for_error(&error),
                &format!("Failed to create S3 client: {error}"),
                server_version,
            );
        }
    };

    let result = run_watch_loop(&client, &request, policy, cancellation, |event| {
        emit_watch_event(&formatter, event).map(|_| ())
    })
    .await;

    match result {
        Ok(WatchTermination::Cancelled) => ExitCode::Interrupted,
        Ok(WatchTermination::OutputClosed) => ExitCode::Success,
        Err(error) => fail_watch(
            &formatter,
            exit_code_for_error(&error),
            &error.to_string(),
            server_version,
        ),
    }
}

async fn run_watch_loop<F>(
    api: &dyn WatchApi,
    request: &WatchRequest,
    policy: ReconnectPolicy,
    mut cancellation: Pin<Box<dyn Future<Output = ()> + Send>>,
    mut on_event: F,
) -> Result<WatchTermination, Error>
where
    F: FnMut(&WatchEvent) -> io::Result<()>,
{
    let mut reconnects = 0_u32;

    loop {
        let opened = tokio::select! {
            biased;
            _ = cancellation.as_mut() => return Ok(WatchTermination::Cancelled),
            opened = api.watch(request) => opened,
        };

        let mut stream = match opened {
            Ok(stream) => stream,
            Err(error) => {
                if !matches!(error, Error::Network(_)) || reconnects >= policy.max_reconnects {
                    return Err(error);
                }
                reconnects += 1;
                if wait_for_reconnect(policy.delay_for(reconnects), cancellation.as_mut()).await {
                    return Ok(WatchTermination::Cancelled);
                }
                continue;
            }
        };

        let disconnect = loop {
            let frame = tokio::select! {
                biased;
                _ = cancellation.as_mut() => return Ok(WatchTermination::Cancelled),
                frame = stream.next() => frame,
            };

            match frame {
                Some(Ok(WatchFrame::Event(event))) => match on_event(&event) {
                    Ok(()) => reconnects = 0,
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                        return Ok(WatchTermination::OutputClosed);
                    }
                    Err(error) => return Err(Error::Io(error)),
                },
                Some(Ok(WatchFrame::KeepAlive)) => reconnects = 0,
                Some(Err(error)) => break error,
                None => {
                    break Error::Network("Watch stream disconnected".to_string());
                }
            }
        };

        if !matches!(disconnect, Error::Network(_)) || reconnects >= policy.max_reconnects {
            return Err(disconnect);
        }
        reconnects += 1;
        if wait_for_reconnect(policy.delay_for(reconnects), cancellation.as_mut()).await {
            return Ok(WatchTermination::Cancelled);
        }
    }
}

async fn wait_for_reconnect(
    delay: Duration,
    mut cancellation: Pin<&mut (dyn Future<Output = ()> + Send)>,
) -> bool {
    tokio::select! {
        biased;
        _ = cancellation.as_mut() => true,
        _ = tokio::time::sleep(delay) => false,
    }
}

fn build_watch_request(args: &WatchArgs) -> Result<(String, WatchRequest), String> {
    let (alias, bucket) = parse_watch_path(&args.path)?;
    validate_filter("prefix", args.prefix.as_deref())?;
    validate_filter("suffix", args.suffix.as_deref())?;

    Ok((
        alias,
        WatchRequest {
            bucket,
            events: normalize_events(&args.events)?,
            prefix: args.prefix.clone().filter(|value| !value.is_empty()),
            suffix: args.suffix.clone().filter(|value| !value.is_empty()),
            ping_seconds: args.ping,
        },
    ))
}

fn parse_watch_path(path: &str) -> Result<(String, Option<String>), String> {
    let normalized = path.strip_suffix('/').unwrap_or(path);
    if normalized.is_empty() || normalized.ends_with('/') {
        return Err("Watch path must include an alias".to_string());
    }

    let mut parts = normalized.split('/');
    let alias = parts.next().unwrap_or_default();
    let bucket = parts.next();
    if alias.is_empty() || parts.next().is_some() {
        return Err(format!(
            "Invalid watch path '{path}'. Expected ALIAS or ALIAS/BUCKET"
        ));
    }
    if bucket.is_some_and(str::is_empty) {
        return Err(format!(
            "Invalid watch path '{path}'. Expected ALIAS or ALIAS/BUCKET"
        ));
    }

    Ok((alias.to_string(), bucket.map(str::to_string)))
}

fn normalize_events(values: &[String]) -> Result<Vec<String>, String> {
    let provided = if values.is_empty() {
        ["put", "delete", "get"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    } else {
        values.to_vec()
    };

    let mut events = Vec::new();
    for value in provided.iter().flat_map(|value| value.split(',')) {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let event = match value.to_ascii_lowercase().as_str() {
            "put" => "s3:ObjectCreated:*".to_string(),
            "delete" => "s3:ObjectRemoved:*".to_string(),
            "get" => "s3:ObjectAccessed:*".to_string(),
            _ if value.starts_with("s3:") => value.to_string(),
            _ => {
                return Err(format!(
                    "Invalid watch event '{value}'. Use put, delete, get, or an s3: event name"
                ));
            }
        };
        if !events.contains(&event) {
            events.push(event);
        }
    }

    if events.is_empty() {
        return Err("At least one non-empty --event value is required".to_string());
    }
    Ok(events)
}

fn validate_filter(name: &str, value: Option<&str>) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.len() > 1024 {
        return Err(format!("--{name} must not exceed 1024 UTF-8 bytes"));
    }
    Ok(())
}

fn validate_watch_capability(report: &CapabilityReport) -> Result<(), (ExitCode, String)> {
    let Some(capability) = report
        .capabilities
        .iter()
        .find(|capability| capability.name == "listen_notification")
    else {
        // A missing declaration on an unknown future server is not proof of absence. The
        // signed S3 request remains authoritative in that case.
        return Ok(());
    };

    match capability.availability {
        CapabilityAvailability::Available | CapabilityAvailability::Unknown => Ok(()),
        CapabilityAvailability::PermissionDenied => Err((
            ExitCode::AuthError,
            capability
                .reason
                .clone()
                .unwrap_or_else(|| "Permission denied for live notifications".to_string()),
        )),
        CapabilityAvailability::Stubbed
        | CapabilityAvailability::Unsupported
        | CapabilityAvailability::Disabled
        | CapabilityAvailability::VersionGated => Err((
            ExitCode::UnsupportedFeature,
            capability
                .reason
                .clone()
                .unwrap_or_else(|| "Live notifications are unavailable".to_string()),
        )),
    }
}

fn emit_watch_event(formatter: &Formatter, event: &WatchEvent) -> io::Result<bool> {
    if formatter.is_quiet() {
        return Ok(false);
    }

    if formatter.is_json() {
        formatter.try_json_line(&WatchSuccessOutput {
            schema_version: 3,
            output_type: "watch_event",
            status: "success",
            data: WatchSuccessData {
                event,
                keepalive: false,
            },
        })?;
        return Ok(true);
    }

    formatter.try_println(&render_human_event(formatter, event))?;
    Ok(true)
}

fn render_human_event(formatter: &Formatter, event: &WatchEvent) -> String {
    let mut fields = vec![
        formatter.style_date(&event.event_time.to_string()),
        formatter.sanitize_text(&event.event_name),
        format!(
            "{}/{}",
            formatter.style_name(&event.bucket),
            formatter.style_file(&event.key)
        ),
    ];
    if let Some(version_id) = &event.version_id {
        fields.push(format!("version={}", formatter.sanitize_text(version_id)));
    }
    if event.delete_marker {
        fields.push("delete-marker".to_string());
    }
    if let Some(event_id) = &event.event_id {
        fields.push(format!("id={}", formatter.sanitize_text(event_id)));
    }
    if let Some(source) = &event.source {
        if let Some(host) = &source.host {
            let mut origin = formatter.sanitize_text(host);
            if let Some(port) = &source.port {
                origin.push(':');
                origin.push_str(&formatter.sanitize_text(port));
            }
            fields.push(format!("source={origin}"));
        }
        if let Some(principal) = &source.principal_id {
            fields.push(format!("principal={}", formatter.sanitize_text(principal)));
        }
        if let Some(user_agent) = &source.user_agent {
            fields.push(format!("agent={}", formatter.sanitize_text(user_agent)));
        }
    }
    fields.join(" ")
}

fn fail_watch(
    formatter: &Formatter,
    code: ExitCode,
    message: &str,
    server: Option<String>,
) -> ExitCode {
    if formatter.is_json() {
        let error = if code == ExitCode::UnsupportedFeature {
            WatchErrorBody::Unsupported(WatchUnsupportedError {
                error_type: "unsupported_feature",
                message: message.to_string(),
                retryable: false,
                capability: "listen_notification",
                server,
                suggestion: None,
            })
        } else {
            let (error_type, retryable, suggestion) = watch_error_metadata(code);
            WatchErrorBody::Standard(WatchStandardError {
                error_type,
                message: message.to_string(),
                retryable,
                suggestion,
            })
        };
        formatter.json_line_error(&WatchErrorOutput {
            schema_version: 3,
            output_type: "watch_event",
            status: "error",
            error,
        });
        code
    } else {
        formatter.fail(code, message)
    }
}

fn exit_code_for_error(error: &Error) -> ExitCode {
    ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError)
}

const fn watch_error_metadata(code: ExitCode) -> (&'static str, bool, Option<&'static str>) {
    match code {
        ExitCode::UsageError => (
            "usage_error",
            false,
            Some("Run `rc watch --help` and verify the scope and filters."),
        ),
        ExitCode::NetworkError => (
            "network_error",
            true,
            Some("Verify the endpoint and retry the watch command."),
        ),
        ExitCode::AuthError => (
            "auth_error",
            false,
            Some("Verify credentials and s3:ListenNotification permission."),
        ),
        ExitCode::NotFound => (
            "not_found",
            false,
            Some("Verify the alias and bucket scope."),
        ),
        ExitCode::Conflict => ("conflict", false, None),
        ExitCode::Interrupted => ("interrupted", true, None),
        ExitCode::Success | ExitCode::GeneralError | ExitCode::UnsupportedFeature => {
            ("general_error", false, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use futures::{StreamExt as _, stream};
    use rc_core::admin::{CapabilityEntry, ClusterSnapshotMetadata};
    use rc_core::{Result as CoreResult, WatchSource, WatchStream};

    use super::*;

    enum OpenResult {
        Frames(Vec<CoreResult<WatchFrame>>, bool),
        Error(Error),
    }

    struct SequenceWatchApi {
        opens: Mutex<VecDeque<OpenResult>>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl WatchApi for SequenceWatchApi {
        async fn watch(&self, _request: &WatchRequest) -> CoreResult<WatchStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let result = self
                .opens
                .lock()
                .expect("sequence lock")
                .pop_front()
                .expect("test must provide an open result");
            match result {
                OpenResult::Frames(frames, stays_open) => {
                    let frames = stream::iter(frames);
                    if stays_open {
                        Ok(Box::pin(frames.chain(stream::pending())))
                    } else {
                        Ok(Box::pin(frames))
                    }
                }
                OpenResult::Error(error) => Err(error),
            }
        }
    }

    struct PendingWatchApi;

    #[async_trait]
    impl WatchApi for PendingWatchApi {
        async fn watch(&self, _request: &WatchRequest) -> CoreResult<WatchStream> {
            std::future::pending().await
        }
    }

    fn args(path: &str) -> WatchArgs {
        WatchArgs {
            path: path.to_string(),
            events: Vec::new(),
            prefix: None,
            suffix: None,
            ping: 1,
            reconnect_attempts: 1,
            reconnect_delay_ms: 1,
            reconnect_max_delay_ms: 2,
        }
    }

    fn event(key: &str) -> WatchEvent {
        WatchEvent {
            event_id: Some("event-1".to_string()),
            event_name: "s3:ObjectCreated:Put".to_string(),
            bucket: "photos".to_string(),
            key: key.to_string(),
            version_id: Some("v1".to_string()),
            delete_marker: false,
            size_bytes: Some(2048),
            etag: Some("abc123".to_string()),
            event_time: "2026-07-21T04:00:00Z"
                .parse()
                .expect("timestamp should parse"),
            source: Some(WatchSource {
                host: Some("node-1".to_string()),
                port: Some("9000".to_string()),
                ..WatchSource::default()
            }),
        }
    }

    fn report(availability: CapabilityAvailability) -> CapabilityReport {
        CapabilityReport {
            server_version: Some("1.0.0-beta.10".to_string()),
            runtime_path: "/runtime".to_string(),
            extensions_path: "/extensions".to_string(),
            cluster_snapshot_path: "/cluster".to_string(),
            capabilities: vec![CapabilityEntry {
                name: "listen_notification".to_string(),
                availability,
                reason: Some("explicit server state".to_string()),
            }],
            extensions: Vec::new(),
            cluster: ClusterSnapshotMetadata {
                summary: None,
                runtime_capabilities_path: None,
                extensions_catalog_path: None,
            },
        }
    }

    #[test]
    fn watch_path_accepts_root_and_bucket_but_rejects_object_scope() {
        assert_eq!(
            parse_watch_path("local/").expect("root scope"),
            ("local".to_string(), None)
        );
        assert_eq!(
            parse_watch_path("local/photos").expect("bucket scope"),
            ("local".to_string(), Some("photos".to_string()))
        );
        assert!(parse_watch_path("local/photos/object").is_err());
        assert!(parse_watch_path("local//").is_err());
        assert!(parse_watch_path("").is_err());
    }

    #[test]
    fn event_filters_are_mc_compatible_repeatable_and_comma_separated() {
        assert_eq!(
            normalize_events(&[]).expect("defaults"),
            vec![
                "s3:ObjectCreated:*",
                "s3:ObjectRemoved:*",
                "s3:ObjectAccessed:*"
            ]
        );
        assert_eq!(
            normalize_events(&["put,delete".to_string(), "put".to_string()])
                .expect("explicit events"),
            vec!["s3:ObjectCreated:*", "s3:ObjectRemoved:*"]
        );
        assert!(normalize_events(&["unknown".to_string()]).is_err());
    }

    #[test]
    fn object_key_filters_are_not_treated_as_local_paths() {
        assert!(validate_filter("prefix", Some("../archive")).is_ok());
        assert!(validate_filter("suffix", Some(r"a\b")).is_ok());
        assert!(validate_filter("prefix", Some(&"x".repeat(1024))).is_ok());
        assert!(validate_filter("prefix", Some(&"x".repeat(1025))).is_err());
    }

    #[test]
    fn capability_gate_rejects_explicit_states_but_not_unknown_servers() {
        assert!(validate_watch_capability(&report(CapabilityAvailability::Available)).is_ok());
        assert!(validate_watch_capability(&report(CapabilityAvailability::Unknown)).is_ok());
        assert_eq!(
            validate_watch_capability(&report(CapabilityAvailability::PermissionDenied))
                .expect_err("permission state should fail")
                .0,
            ExitCode::AuthError
        );
        assert_eq!(
            validate_watch_capability(&report(CapabilityAvailability::Unsupported))
                .expect_err("unsupported state should fail")
                .0,
            ExitCode::UnsupportedFeature
        );

        let mut future = report(CapabilityAvailability::Available);
        future.capabilities.clear();
        assert!(validate_watch_capability(&future).is_ok());
    }

    #[test]
    fn reconnect_backoff_is_exponential_and_bounded() {
        let policy = ReconnectPolicy {
            max_reconnects: 10,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(350),
        };

        assert_eq!(policy.delay_for(1), Duration::from_millis(100));
        assert_eq!(policy.delay_for(2), Duration::from_millis(200));
        assert_eq!(policy.delay_for(3), Duration::from_millis(350));
        assert_eq!(policy.delay_for(u32::MAX), Duration::from_millis(350));
    }

    #[tokio::test]
    async fn disconnect_reconnects_and_cancellation_stops_the_live_stream() {
        let api = SequenceWatchApi {
            opens: Mutex::new(VecDeque::from([
                OpenResult::Frames(Vec::new(), false),
                OpenResult::Frames(
                    vec![Ok(WatchFrame::Event(Box::new(event("image.jpg"))))],
                    true,
                ),
            ])),
            calls: AtomicUsize::new(0),
        };
        let notify = Arc::new(tokio::sync::Notify::new());
        let cancellation_notify = Arc::clone(&notify);
        let cancellation = Box::pin(async move { cancellation_notify.notified().await });
        let mut keys = Vec::new();

        let result = run_watch_loop(
            &api,
            &build_watch_request(&args("local/photos"))
                .expect("request")
                .1,
            ReconnectPolicy {
                max_reconnects: 1,
                initial_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            },
            cancellation,
            |event| {
                keys.push(event.key.clone());
                notify.notify_one();
                Ok(())
            },
        )
        .await
        .expect("cancellation should be clean");

        assert_eq!(result, WatchTermination::Cancelled);
        assert_eq!(keys, vec!["image.jpg"]);
        assert_eq!(api.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn healthy_frame_resets_the_consecutive_reconnect_budget() {
        let api = SequenceWatchApi {
            opens: Mutex::new(VecDeque::from([
                OpenResult::Error(Error::Network("first failure".to_string())),
                OpenResult::Frames(
                    vec![Ok(WatchFrame::Event(Box::new(event("first.jpg"))))],
                    false,
                ),
                OpenResult::Frames(
                    vec![Ok(WatchFrame::Event(Box::new(event("second.jpg"))))],
                    true,
                ),
            ])),
            calls: AtomicUsize::new(0),
        };
        let notify = Arc::new(tokio::sync::Notify::new());
        let cancellation_notify = Arc::clone(&notify);
        let mut keys = Vec::new();

        let result = run_watch_loop(
            &api,
            &build_watch_request(&args("local/photos"))
                .expect("request")
                .1,
            ReconnectPolicy {
                max_reconnects: 1,
                initial_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            },
            Box::pin(async move { cancellation_notify.notified().await }),
            |event| {
                keys.push(event.key.clone());
                if event.key == "second.jpg" {
                    notify.notify_one();
                }
                Ok(())
            },
        )
        .await
        .expect("isolated failures should remain recoverable");

        assert_eq!(result, WatchTermination::Cancelled);
        assert_eq!(keys, vec!["first.jpg", "second.jpg"]);
        assert_eq!(api.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn broken_output_pipe_stops_watch_cleanly() {
        let api = SequenceWatchApi {
            opens: Mutex::new(VecDeque::from([OpenResult::Frames(
                vec![Ok(WatchFrame::Event(Box::new(event("image.jpg"))))],
                true,
            )])),
            calls: AtomicUsize::new(0),
        };

        let result = run_watch_loop(
            &api,
            &build_watch_request(&args("local/photos"))
                .expect("request")
                .1,
            ReconnectPolicy {
                max_reconnects: 1,
                initial_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            },
            Box::pin(std::future::pending()),
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "reader closed",
                ))
            },
        )
        .await
        .expect("broken stdout should be a clean termination");

        assert_eq!(result, WatchTermination::OutputClosed);
        assert_eq!(api.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exhausted_retries_return_the_last_network_error() {
        let api = SequenceWatchApi {
            opens: Mutex::new(VecDeque::from([
                OpenResult::Error(Error::Network("first disconnect".to_string())),
                OpenResult::Error(Error::Network("second disconnect".to_string())),
            ])),
            calls: AtomicUsize::new(0),
        };

        let error = run_watch_loop(
            &api,
            &build_watch_request(&args("local/")).expect("request").1,
            ReconnectPolicy {
                max_reconnects: 1,
                initial_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            },
            Box::pin(std::future::pending()),
            |_| Ok(()),
        )
        .await
        .expect_err("retry budget should be exhausted");

        assert!(matches!(error, Error::Network(_)));
        assert!(error.to_string().contains("second disconnect"));
        assert_eq!(api.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_pending_connection_attempt() {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            run_watch_loop(
                &PendingWatchApi,
                &build_watch_request(&args("local/")).expect("request").1,
                ReconnectPolicy {
                    max_reconnects: 0,
                    initial_delay: Duration::ZERO,
                    max_delay: Duration::ZERO,
                },
                Box::pin(std::future::ready(())),
                |_| Ok(()),
            ),
        )
        .await
        .expect("cancellation should be prompt")
        .expect("cancellation should be clean");

        assert_eq!(result, WatchTermination::Cancelled);
    }

    #[test]
    fn compact_watch_json_line_validates_against_output_v3() {
        let sample = event("image.jpg");
        let output = WatchSuccessOutput {
            schema_version: 3,
            output_type: "watch_event",
            status: "success",
            data: WatchSuccessData {
                event: &sample,
                keepalive: false,
            },
        };
        let line = serde_json::to_string(&output).expect("watch output should serialize");
        assert!(!line.contains('\n'));

        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../../schemas/output_v3.json"))
                .expect("v3 schema should parse");
        let validator = jsonschema::validator_for(&schema).expect("v3 schema should compile");
        let value = serde_json::from_str(&line).expect("watch line should parse");
        let errors = validator
            .iter_errors(&value)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert!(errors.is_empty(), "watch JSONL failed schema: {errors:?}");
    }

    #[test]
    fn compact_watch_error_lines_validate_against_output_v3() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../../schemas/output_v3.json"))
                .expect("v3 schema should parse");
        let validator = jsonschema::validator_for(&schema).expect("v3 schema should compile");
        let outputs = [
            WatchErrorOutput {
                schema_version: 3,
                output_type: "watch_event",
                status: "error",
                error: WatchErrorBody::Unsupported(WatchUnsupportedError {
                    error_type: "unsupported_feature",
                    message: "Live notifications are unavailable".to_string(),
                    retryable: false,
                    capability: "listen_notification",
                    server: Some("1.0.0-beta.9".to_string()),
                    suggestion: None,
                }),
            },
            WatchErrorOutput {
                schema_version: 3,
                output_type: "watch_event",
                status: "error",
                error: WatchErrorBody::Standard(WatchStandardError {
                    error_type: "network_error",
                    message: "Watch stream disconnected".to_string(),
                    retryable: true,
                    suggestion: Some("Retry the command."),
                }),
            },
        ];

        for output in outputs {
            let line = serde_json::to_string(&output).expect("watch error should serialize");
            assert!(!line.contains('\n'));
            let value = serde_json::from_str(&line).expect("watch error should parse");
            let errors = validator
                .iter_errors(&value)
                .map(|error| error.to_string())
                .collect::<Vec<_>>();
            assert!(errors.is_empty(), "watch error failed schema: {errors:?}");
        }
    }

    #[test]
    fn human_output_sanitizes_server_controlled_fields() {
        let formatter = Formatter::new(OutputConfig {
            json: false,
            no_color: true,
            no_progress: true,
            quiet: false,
        });
        let mut malicious = event("safe\nforged");
        malicious.event_name = "put\u{1b}[31m".to_string();
        malicious.source = Some(WatchSource {
            user_agent: Some("agent\rforged".to_string()),
            ..WatchSource::default()
        });

        let rendered = render_human_event(&formatter, &malicious);
        assert!(!rendered.contains('\n'));
        assert!(!rendered.contains('\r'));
        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.contains("safe\\nforged"));
        assert!(rendered.contains("\\u{1b}"));
    }

    #[test]
    fn quiet_output_suppresses_watch_events() {
        let formatter = Formatter::new(OutputConfig {
            json: true,
            no_color: true,
            no_progress: true,
            quiet: true,
        });

        assert!(!emit_watch_event(&formatter, &event("image.jpg")).expect("quiet output"));
    }

    #[test]
    fn watch_errors_cover_stable_exit_code_categories() {
        assert_eq!(
            exit_code_for_error(&Error::InvalidPath("bad filter".to_string())),
            ExitCode::UsageError
        );
        assert_eq!(
            exit_code_for_error(&Error::Network("disconnected".to_string())),
            ExitCode::NetworkError
        );
        assert_eq!(
            exit_code_for_error(&Error::Auth("denied".to_string())),
            ExitCode::AuthError
        );
        assert_eq!(
            exit_code_for_error(&Error::UnsupportedFeature("missing route".to_string())),
            ExitCode::UnsupportedFeature
        );
    }
}
