//! Retrieve and locally verify an encrypted RustFS diagnostic archive.

use std::future::Future;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use clap::Args;
use rc_core::admin::{
    CapabilityApi, CapabilityAvailability, DiagnosticCapability, InspectArchiveApi,
    InspectArchiveCancellation, InspectArchiveKey, InspectArchiveTransportRequest,
    PublishedInspectArchive, decrypt_and_validate_inspect_archive_with_cancel,
    publish_inspect_archive, validate_inspect_archive_output_directory,
};
use rc_core::{Error, parse_object_path};
use serde::Serialize;
use zeroize::Zeroizing;

use super::super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;

const MAX_PRIVATE_KEY_BYTES: u64 = 64 * 1024;

/// Options for retrieving one encrypted diagnostic metadata archive.
#[derive(Args, Debug, Clone)]
pub struct InspectArchiveArgs {
    /// Exact RustFS object target (ALIAS/BUCKET/OBJECT)
    pub target: String,

    /// Destination for the verified plaintext tar archive
    #[arg(short, long)]
    pub output: PathBuf,

    /// Read a caller-managed PKCS#8 RSA private key from a protected file
    #[arg(long, value_name = "FILE")]
    pub private_key: Option<PathBuf>,
}

pub(super) async fn execute_inspect_archive(
    args: InspectArchiveArgs,
    formatter: &Formatter,
) -> ExitCode {
    let target = match validate_args(&args) {
        Ok(target) => target,
        Err(error) => return emit_archive_error(&error, None, formatter),
    };
    let client = match get_admin_client(&target.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    run_inspect_archive(
        args,
        target.bucket,
        target.key,
        &client,
        &client,
        formatter,
        tokio::signal::ctrl_c(),
    )
    .await
}

fn validate_args(args: &InspectArchiveArgs) -> rc_core::Result<rc_core::RemotePath> {
    let target = parse_object_path(&args.target)?;
    if target
        .key
        .split('/')
        .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(Error::InvalidPath(
            "Diagnostic archive object contains an unsafe path component".to_string(),
        ));
    }
    if args.output.as_os_str().is_empty() {
        return Err(Error::InvalidPath(
            "Diagnostic archive output path cannot be empty".to_string(),
        ));
    }
    if args
        .output
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(Error::InvalidPath(
            "Diagnostic archive output cannot contain parent-directory components".to_string(),
        ));
    }
    if args.output.exists() {
        return Err(Error::Conflict(
            "Diagnostic archive output already exists".to_string(),
        ));
    }
    let parent = output_parent(&args.output);
    validate_inspect_archive_output_directory(parent)?;
    Ok(target)
}

fn output_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

async fn run_inspect_archive<F>(
    args: InspectArchiveArgs,
    bucket: String,
    object: String,
    capabilities: &dyn CapabilityApi,
    api: &dyn InspectArchiveApi,
    formatter: &Formatter,
    interrupt: F,
) -> ExitCode
where
    F: Future<Output = std::io::Result<()>>,
{
    let report = match capabilities.discover_capabilities(false).await {
        Ok(report) => report,
        Err(error) => return emit_archive_error(&error, None, formatter),
    };
    if let Err(error) = report.require_diagnostic_capability(DiagnosticCapability::InspectArchive) {
        let (mapped, kind) = match error.availability() {
            CapabilityAvailability::PermissionDenied => (
                Error::Auth("Permission denied during capability discovery".to_string()),
                Some("auth_error"),
            ),
            CapabilityAvailability::Disabled => (
                Error::RequestRejected(
                    error
                        .reason()
                        .unwrap_or("Diagnostic archive capability is disabled")
                        .to_string(),
                ),
                Some("disabled"),
            ),
            _ => (
                Error::UnsupportedFeature(error.to_string()),
                Some("unsupported_feature"),
            ),
        };
        return emit_archive_error(&mapped, kind, formatter);
    }
    let contract = match api.inspect_archive_capability().await {
        Ok(contract) => contract,
        Err(error) => return emit_archive_error(&error, None, formatter),
    };
    if let Err(error) = contract.validate() {
        let kind = (contract.state.availability() == CapabilityAvailability::Disabled)
            .then_some("disabled");
        return emit_archive_error(&error, kind, formatter);
    }

    let private_key_path = args.private_key.clone();
    let key = match tokio::task::spawn_blocking(move || {
        if let Some(path) = private_key_path {
            let pem = read_private_key(&path)?;
            InspectArchiveKey::from_pkcs8_pem(&pem)
        } else {
            InspectArchiveKey::generate()
        }
    })
    .await
    {
        Ok(Ok(key)) => key,
        Ok(Err(error)) => return emit_archive_error(&error, None, formatter),
        Err(_) => {
            return emit_archive_error(
                &Error::General("Diagnostic archive key preparation failed".to_string()),
                None,
                formatter,
            );
        }
    };

    let temporary_directory = output_parent(&args.output).to_path_buf();
    let request = InspectArchiveTransportRequest {
        bucket,
        object,
        public_key_pem: key.public_key_pem().to_string(),
        max_bytes: contract.max_bytes,
        timeout: contract.timeout(),
    };
    tokio::pin!(interrupt);
    let encrypted = tokio::select! {
        biased;
        interrupt_result = &mut interrupt => {
            return interrupt_result.map_or_else(
                |_| emit_archive_error(
                    &Error::General("Failed to register diagnostic archive interruption handler".to_string()),
                    None,
                    formatter,
                ),
                |_| emit_archive_error(
                    &Error::Interrupted("Diagnostic archive retrieval was cancelled".to_string()),
                    Some("interrupted"),
                    formatter,
                ),
            );
        }
        result = api.download_inspect_archive(request, &temporary_directory) => {
            match result {
                Ok(encrypted) => encrypted,
                Err(error) => return emit_archive_error(&error, None, formatter),
            }
        }
    };

    let cancellation = InspectArchiveCancellation::default();
    let blocking_cancellation = cancellation.clone();
    let maximum = contract.max_bytes;
    let maximum_metadata = contract.max_metadata_bytes_per_drive;
    let directory = temporary_directory.clone();
    let verification = tokio::task::spawn_blocking(move || {
        decrypt_and_validate_inspect_archive_with_cancel(
            encrypted,
            &key,
            &directory,
            maximum,
            maximum_metadata,
            maximum,
            &blocking_cancellation,
        )
    });
    tokio::pin!(verification);
    let verified = tokio::select! {
        biased;
        interrupt_result = &mut interrupt => {
            cancellation.cancel();
            let _ = verification.await;
            return interrupt_result.map_or_else(
                |_| emit_archive_error(
                    &Error::General("Failed to monitor diagnostic archive interruption".to_string()),
                    None,
                    formatter,
                ),
                |_| emit_archive_error(
                    &Error::Interrupted("Diagnostic archive verification was cancelled".to_string()),
                    Some("interrupted"),
                    formatter,
                ),
            );
        }
        result = &mut verification => {
            match result {
                Ok(Ok(verified)) => verified,
                Ok(Err(error)) => return emit_archive_error(&error, None, formatter),
                Err(_) => return emit_archive_error(
                    &Error::General("Diagnostic archive verification task failed".to_string()),
                    Some("cryptographic_verification"),
                    formatter,
                ),
            }
        }
    };

    match publish_inspect_archive(verified, &args.output) {
        Ok(published) => {
            emit_archive_success(published, formatter);
            ExitCode::Success
        }
        Err(error) => emit_archive_error(&error, Some("local_io"), formatter),
    }
}

fn read_private_key(path: &Path) -> rc_core::Result<Zeroizing<String>> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| {
        Error::InvalidPath("Failed to inspect diagnostic archive private-key file".to_string())
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::InvalidPath(
            "Diagnostic archive private key must be a regular file, not a symlink".to_string(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::InvalidPath(
                "Diagnostic archive private key cannot grant group or other permissions"
                    .to_string(),
            ));
        }
    }
    let file = std::fs::File::open(path).map_err(|_| {
        Error::InvalidPath("Failed to open diagnostic archive private-key file".to_string())
    })?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_PRIVATE_KEY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            Error::InvalidPath("Failed to read diagnostic archive private-key file".to_string())
        })?;
    if bytes.len() as u64 > MAX_PRIVATE_KEY_BYTES {
        return Err(Error::InvalidPath(
            "Diagnostic archive private-key file exceeds the 64 KiB limit".to_string(),
        ));
    }
    std::str::from_utf8(&bytes)
        .map(|pem| Zeroizing::new(pem.to_string()))
        .map_err(|_| {
            Error::InvalidPath(
                "Diagnostic archive private-key file must contain UTF-8 PEM".to_string(),
            )
        })
}

#[derive(Debug, Serialize)]
struct ArchiveSuccessOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: ArchiveSuccessData,
}

#[derive(Debug, Serialize)]
struct ArchiveSuccessData {
    operation: &'static str,
    state: &'static str,
    output: PathBuf,
    archive_version: u16,
    drive_count: usize,
    encrypted_bytes: u64,
    plaintext_bytes: u64,
    plaintext_sha256: String,
}

fn emit_archive_success(published: PublishedInspectArchive, formatter: &Formatter) {
    if formatter.is_json() {
        formatter.json(&ArchiveSuccessOutput {
            schema_version: 3,
            output_type: "diagnostic_archive",
            status: "success",
            data: ArchiveSuccessData {
                operation: "retrieve",
                state: "verified",
                output: published.path,
                archive_version: published.archive_version,
                drive_count: published.drive_count,
                encrypted_bytes: published.encrypted_bytes,
                plaintext_bytes: published.plaintext_bytes,
                plaintext_sha256: published.plaintext_sha256,
            },
        });
    } else {
        formatter.success(&format!(
            "Verified diagnostic archive written to '{}' (version {}, {} drive artifacts, {} bytes)",
            formatter.sanitize_text(&published.path.display().to_string()),
            published.archive_version,
            published.drive_count,
            published.plaintext_bytes
        ));
    }
}

#[derive(Debug, Serialize)]
struct ArchiveErrorOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    error: ArchiveErrorBody,
}

#[derive(Debug, Serialize)]
struct ArchiveErrorBody {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    capability: Option<&'static str>,
    server: Option<&'static str>,
    suggestion: Option<&'static str>,
}

fn emit_archive_error(
    error: &Error,
    override_kind: Option<&'static str>,
    formatter: &Formatter,
) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    let error_type = override_kind.unwrap_or_else(|| archive_error_type(error));
    let message = format!("Diagnostic archive operation failed: {error}");
    if formatter.is_json() {
        formatter.json_error(&ArchiveErrorOutput {
            schema_version: 3,
            output_type: "diagnostic_archive",
            status: "error",
            error: ArchiveErrorBody {
                error_type,
                message,
                retryable: matches!(error, Error::Network(_) | Error::Interrupted(_)),
                capability: matches!(error, Error::UnsupportedFeature(_))
                    .then_some(rc_core::admin::INSPECT_ARCHIVE_CAPABILITY),
                server: matches!(error, Error::UnsupportedFeature(_)).then_some("rustfs"),
                suggestion: archive_error_suggestion(error_type),
            },
        });
    } else {
        formatter.error_with_code(code, &message);
    }
    code
}

fn archive_error_type(error: &Error) -> &'static str {
    match error {
        Error::InvalidPath(message)
            if message.contains("staging")
                || message.contains("publish")
                || message.contains("sync") =>
        {
            "local_io"
        }
        Error::InvalidPath(_) | Error::Config(_) => "invalid_target",
        Error::Network(_) => "network_error",
        Error::Auth(_) => "auth_error",
        Error::NotFound(_) | Error::AliasNotFound(_) => "not_found",
        Error::Conflict(_) | Error::AliasExists(_) => "conflict",
        Error::UnsupportedFeature(_) => "unsupported_feature",
        Error::RequestRejected(_) => "server_limit",
        Error::Interrupted(_) => "interrupted",
        Error::General(message)
            if message.starts_with("Diagnostic archive verification failed") =>
        {
            "cryptographic_verification"
        }
        Error::Io(_) => "local_io",
        _ => "general_error",
    }
}

fn archive_error_suggestion(kind: &str) -> Option<&'static str> {
    match kind {
        "invalid_target" => Some("Verify the exact object target, output path, and private key."),
        "auth_error" => Some("Verify credentials include the InspectData admin permission."),
        "unsupported_feature" => {
            Some("Upgrade RustFS to a release with encrypted inspect archives.")
        }
        "disabled" => Some("Initialize local RustFS drives before requesting an archive."),
        "network_error" => Some("Verify connectivity and retry; incomplete files were removed."),
        "server_limit" => {
            Some("Retry after server capacity is available or reduce target metadata.")
        }
        "cryptographic_verification" => Some("Do not use the output; retrieve a fresh archive."),
        "local_io" => Some("Verify destination permissions and available disk space."),
        "interrupted" => Some("Retry when ready; incomplete files were removed."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::future::{pending, ready};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use jsonschema::Validator;
    use rc_core::Result;
    use rc_core::admin::{
        CapabilityEntry, CapabilityReport, ClusterSnapshotMetadata, EncryptedInspectArchive,
        INSPECT_ARCHIVE_COMPLETION, INSPECT_ARCHIVE_CONTENT_TYPE, INSPECT_ARCHIVE_ENCRYPTION,
        INSPECT_ARCHIVE_ROUTE, InspectArchiveCapabilityContract, RuntimeCapabilityState,
        RuntimeCapabilityStatus,
    };
    use serde_json::Value;
    use tempfile::tempdir;

    use super::*;
    use crate::output::OutputConfig;

    struct DropMarker(Arc<AtomicBool>);

    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct StubArchiveApi {
        availability: CapabilityAvailability,
        downloads: AtomicUsize,
        dropped: Arc<AtomicBool>,
    }

    #[async_trait]
    impl CapabilityApi for StubArchiveApi {
        async fn discover_capabilities(&self, _refresh: bool) -> Result<CapabilityReport> {
            Ok(CapabilityReport {
                server_version: Some("test".to_string()),
                runtime_path: "/rustfs/admin/v4/runtime/capabilities".to_string(),
                extensions_path: "/rustfs/admin/v4/extensions/catalog".to_string(),
                cluster_snapshot_path: "/rustfs/admin/v4/cluster/snapshot".to_string(),
                capabilities: vec![CapabilityEntry {
                    name: DiagnosticCapability::InspectArchive.name().to_string(),
                    availability: self.availability,
                    reason: None,
                }],
                extensions: Vec::new(),
                cluster: ClusterSnapshotMetadata {
                    summary: None,
                    runtime_capabilities_path: None,
                    extensions_catalog_path: None,
                },
            })
        }
    }

    #[async_trait]
    impl InspectArchiveApi for StubArchiveApi {
        async fn inspect_archive_capability(&self) -> Result<InspectArchiveCapabilityContract> {
            Ok(contract())
        }

        async fn download_inspect_archive(
            &self,
            _request: InspectArchiveTransportRequest,
            _temporary_directory: &Path,
        ) -> Result<EncryptedInspectArchive> {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            let _drop_marker = DropMarker(Arc::clone(&self.dropped));
            pending::<()>().await;
            Err(Error::General("pending transport completed".to_string()))
        }
    }

    fn contract() -> InspectArchiveCapabilityContract {
        InspectArchiveCapabilityContract {
            state: RuntimeCapabilityStatus {
                state: RuntimeCapabilityState::Supported,
                reason: None,
                extra: BTreeMap::new(),
            },
            route: INSPECT_ARCHIVE_ROUTE.to_string(),
            archive_version: 1,
            content_type: INSPECT_ARCHIVE_CONTENT_TYPE.to_string(),
            encryption: INSPECT_ARCHIVE_ENCRYPTION.to_string(),
            completion_contract: INSPECT_ARCHIVE_COMPLETION.to_string(),
            max_bytes: 16 * 1024 * 1024,
            max_duration_secs: 30,
            max_metadata_bytes_per_drive: 1024 * 1024,
            extra: BTreeMap::new(),
        }
    }

    fn stub(availability: CapabilityAvailability) -> StubArchiveApi {
        StubArchiveApi {
            availability,
            downloads: AtomicUsize::new(0),
            dropped: Arc::new(AtomicBool::new(false)),
        }
    }

    fn formatter() -> Formatter {
        Formatter::new(OutputConfig {
            quiet: true,
            ..OutputConfig::default()
        })
    }

    fn args(directory: &Path) -> InspectArchiveArgs {
        InspectArchiveArgs {
            target: "local/diagnostics/node.json".to_string(),
            output: directory.join("inspect.tar"),
            private_key: None,
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

    #[test]
    fn inspect_archive_rejects_unsafe_targets_and_existing_output() {
        let directory = tempdir().expect("temporary directory");
        for target in ["local/bucket", "local/bucket/a//b", "local/bucket/a/../b"] {
            let mut value = args(directory.path());
            value.target = target.to_string();
            assert!(validate_args(&value).is_err(), "{target} must fail");
        }
        let value = args(directory.path());
        std::fs::write(&value.output, b"existing").expect("existing output");
        assert!(matches!(validate_args(&value), Err(Error::Conflict(_))));
    }

    #[cfg(unix)]
    #[test]
    fn inspect_archive_private_key_requires_regular_private_file() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempdir().expect("temporary directory");
        let key = directory.path().join("key.pem");
        std::fs::write(&key, b"not-a-key").expect("key fixture");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644))
            .expect("public permissions");
        assert!(read_private_key(&key).is_err());

        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))
            .expect("private permissions");
        let link = directory.path().join("key-link.pem");
        symlink(&key, &link).expect("key symlink");
        assert!(read_private_key(&link).is_err());
    }

    #[tokio::test]
    async fn inspect_archive_fails_closed_before_transport_when_unavailable() {
        let directory = tempdir().expect("temporary directory");
        for (availability, expected) in [
            (
                CapabilityAvailability::Unsupported,
                ExitCode::UnsupportedFeature,
            ),
            (CapabilityAvailability::Disabled, ExitCode::GeneralError),
            (
                CapabilityAvailability::PermissionDenied,
                ExitCode::AuthError,
            ),
        ] {
            let api = stub(availability);
            let code = run_inspect_archive(
                args(directory.path()),
                "diagnostics".to_string(),
                "node.json".to_string(),
                &api,
                &api,
                &formatter(),
                pending::<std::io::Result<()>>(),
            )
            .await;
            assert_eq!(code, expected);
            assert_eq!(api.downloads.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn inspect_archive_interrupt_drops_in_flight_transport() {
        let directory = tempdir().expect("temporary directory");
        let api = stub(CapabilityAvailability::Available);
        let dropped = Arc::clone(&api.dropped);
        let interrupt = async {
            tokio::task::yield_now().await;
            Ok(())
        };
        let code = run_inspect_archive(
            args(directory.path()),
            "diagnostics".to_string(),
            "node.json".to_string(),
            &api,
            &api,
            &formatter(),
            interrupt,
        )
        .await;
        assert_eq!(code, ExitCode::Interrupted);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(!directory.path().join("inspect.tar").exists());
    }

    #[tokio::test]
    async fn inspect_archive_ready_interrupt_does_not_start_transport() {
        let directory = tempdir().expect("temporary directory");
        let api = stub(CapabilityAvailability::Available);
        let code = run_inspect_archive(
            args(directory.path()),
            "diagnostics".to_string(),
            "node.json".to_string(),
            &api,
            &api,
            &formatter(),
            ready(Ok(())),
        )
        .await;
        assert_eq!(code, ExitCode::Interrupted);
        assert_eq!(api.downloads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn inspect_archive_json_success_and_error_satisfy_output_v3() {
        let success = ArchiveSuccessOutput {
            schema_version: 3,
            output_type: "diagnostic_archive",
            status: "success",
            data: ArchiveSuccessData {
                operation: "retrieve",
                state: "verified",
                output: PathBuf::from("inspect.tar"),
                archive_version: 1,
                drive_count: 2,
                encrypted_bytes: 2048,
                plaintext_bytes: 1024,
                plaintext_sha256: "a".repeat(64),
            },
        };
        let error = ArchiveErrorOutput {
            schema_version: 3,
            output_type: "diagnostic_archive",
            status: "error",
            error: ArchiveErrorBody {
                error_type: "cryptographic_verification",
                message: "Diagnostic archive verification failed".to_string(),
                retryable: false,
                capability: None,
                server: None,
                suggestion: Some("Do not use the output; retrieve a fresh archive."),
            },
        };
        let validator = output_v3_validator();
        for value in [
            serde_json::to_value(success).expect("success JSON"),
            serde_json::to_value(error).expect("error JSON"),
        ] {
            assert!(validator.is_valid(&value), "invalid output: {value}");
        }
    }
}
