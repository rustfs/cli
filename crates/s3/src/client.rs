//! S3 client implementation
//!
//! Wraps aws-sdk-s3 and implements the ObjectStore trait from rc-core.

use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SignatureLocation, SigningSettings, sign,
};
use aws_sigv4::sign::v4;
use aws_smithy_runtime_api::box_error::BoxError;
use aws_smithy_runtime_api::client::http::{
    HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpConnector,
};
use aws_smithy_runtime_api::client::interceptors::Intercept;
use aws_smithy_runtime_api::client::interceptors::context::BeforeTransmitInterceptorContextMut;
use aws_smithy_runtime_api::client::orchestrator::HttpRequest;
use aws_smithy_runtime_api::client::result::ConnectorError;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_runtime_api::http::{Response, StatusCode};
use aws_smithy_types::body::SdkBody;
use aws_smithy_types::config_bag::ConfigBag;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use bytes::Bytes;
use futures::TryStreamExt as _;
use http_body::Frame;
use http_body_util::StreamBody;
use jiff::Timestamp;
use md5::Md5;
use quick_xml::de::from_str as from_xml_str;
pub use rc_core::DeleteRequestOptions;
use rc_core::admin::KmsDiagnosticStore;
use rc_core::{
    AbortMultipartUploadRequest, Alias, BucketEncryption, BucketNotification,
    BucketObjectLockConfiguration, Capabilities, ChecksumAlgorithm, ChecksumRequest,
    CopyObjectOptions, CorsRule, CreateBucketOptions, DefaultRetention, DeleteObjectFailure,
    DeleteObjectsResult, DeletedObject, Error, LegalHoldStatus, LifecycleRule,
    ListObjectVersionsOptions, ListOptions, ListResult, MetadataDirective, MultipartAbortStatus,
    MultipartCopyCancellation, MultipartCopyOptions, MultipartCopyPlan, MultipartCopyProgress,
    MultipartCopyResult, MultipartIdentity, MultipartUpload, MultipartUploadListOptions,
    MultipartUploadListResult, NotificationTarget, ObjectAttributes, ObjectChecksum,
    ObjectEncryptionRequest, ObjectInfo, ObjectLockOptions, ObjectReadOptions, ObjectRetention,
    ObjectStore, ObjectTransferMetadata, ObjectVersion, ObjectVersionIdentifier,
    ObjectVersionListResult, ObjectWriteEncryption, ObjectWriteOptions, RemotePath,
    ReplicationCheckPhase, ReplicationCheckPhaseState, ReplicationCheckResult,
    ReplicationCheckStatus, ReplicationConfiguration, ReplicationResyncStartOptions,
    ReplicationResyncStartResult, ReplicationResyncState, ReplicationResyncStatus,
    ReplicationResyncTargetStatus, RequestHeader, Result, RetentionDuration, RetentionDurationUnit,
    RetentionMode, SelectOptions, SseCustomerKey, TransferCopyOptions, TransferReadOptions,
    global_request_headers,
};
use reqwest::Method;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

/// Keep single-part uploads small to avoid backend incompatibilities with
/// streaming aws-chunked payloads.
const SINGLE_PUT_OBJECT_MAX_SIZE: u64 = crate::multipart::DEFAULT_PART_SIZE;
const S3_SERVICE_NAME: &str = "s3";
const S3_REPLICATION_XML_NAMESPACE: &str = "http://s3.amazonaws.com/doc/2006-03-01/";
const RUSTFS_FORCE_DELETE_HEADER: &str = "x-rustfs-force-delete";
const REPLICATION_EXTENSION_BODY_LIMIT: u64 = 1024 * 1024;
const REPLICATION_CHECK_PROBE_NAMESPACE: &str = ".rustfs.sys/replication-check/";
const REPLICATION_CHECK_ERROR_LIMIT: usize = 512;
const REPLICATION_CHECK_DESCRIPTION_LIMIT: usize = 1024;

fn contains_control_characters(value: &str) -> bool {
    value.chars().any(char::is_control)
}

fn validate_replication_check_error(error: Option<&str>) -> Result<()> {
    if error.is_some_and(|value| {
        value.is_empty()
            || value.len() > REPLICATION_CHECK_ERROR_LIMIT
            || contains_control_characters(value)
    }) {
        return Err(Error::General(
            "Malformed structured replication check response".to_string(),
        ));
    }
    Ok(())
}

fn validate_replication_check_phase(phase: &ReplicationCheckPhase) -> Result<()> {
    validate_replication_check_error(phase.error.as_deref())?;
    let error_matches_status = match phase.status {
        ReplicationCheckPhaseState::Failed => phase.error.is_some(),
        ReplicationCheckPhaseState::Ok | ReplicationCheckPhaseState::Skipped => {
            phase.error.is_none()
        }
    };
    if !error_matches_status {
        return Err(Error::General(
            "Malformed structured replication check response".to_string(),
        ));
    }
    Ok(())
}

fn replication_check_phases(
    phases: &rc_core::ReplicationCheckPhases,
) -> [&ReplicationCheckPhase; 7] {
    [
        &phases.bucket,
        &phases.versioning,
        &phases.object_lock,
        &phases.put,
        &phases.delete_marker,
        &phases.version_delete,
        &phases.cleanup,
    ]
}

fn replication_check_phases_mut(
    phases: &mut rc_core::ReplicationCheckPhases,
) -> [&mut ReplicationCheckPhase; 7] {
    [
        &mut phases.bucket,
        &mut phases.versioning,
        &mut phases.object_lock,
        &mut phases.put,
        &mut phases.delete_marker,
        &mut phases.version_delete,
        &mut phases.cleanup,
    ]
}

#[derive(Debug, Clone, Copy)]
enum ObjectWritePrecondition<'a> {
    None,
    IfAbsent,
    IfMatch(&'a str),
}

#[derive(Debug, Clone, Copy)]
struct PathUploadOptions<'a> {
    write: &'a ObjectWriteOptions,
    precondition: ObjectWritePrecondition<'a>,
}

struct KmsDiagnosticObjectBody(Zeroizing<Vec<u8>>);

impl AsRef<[u8]> for KmsDiagnosticObjectBody {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BucketPolicyErrorKind {
    MissingPolicy,
    MissingBucket,
    Other,
}

/// Custom HTTP connector using reqwest, supporting insecure TLS (skip cert verification)
/// and custom CA bundles. Used when `alias.insecure = true` or `alias.ca_bundle.is_some()`.
#[derive(Debug, Clone)]
struct ReqwestConnector {
    client: reqwest::Client,
}

impl ReqwestConnector {
    async fn new(
        insecure: bool,
        ca_bundle: Option<&str>,
        client_cert: Option<&str>,
        client_key: Option<&str>,
        timeout: Option<&rc_core::alias::TimeoutConfig>,
    ) -> Result<Self> {
        let client =
            build_reqwest_client(insecure, ca_bundle, client_cert, client_key, timeout).await?;
        Ok(Self { client })
    }
}

async fn build_reqwest_client(
    insecure: bool,
    ca_bundle: Option<&str>,
    client_cert: Option<&str>,
    client_key: Option<&str>,
    timeout: Option<&rc_core::alias::TimeoutConfig>,
) -> Result<reqwest::Client> {
    // NOTE: When `insecure = true`, `danger_accept_invalid_certs` disables all TLS
    // certificate verification. Any CA bundle provided will still be added to the
    // trust store but is rendered ineffective for this connection.
    let mut builder = reqwest::Client::builder()
        .danger_accept_invalid_certs(insecure)
        .redirect(reqwest::redirect::Policy::none());

    if let Some(timeout) = timeout {
        builder = builder
            .connect_timeout(Duration::from_millis(timeout.connect_ms))
            .read_timeout(Duration::from_millis(timeout.read_ms));
    }

    if let Some(bundle_path) = ca_bundle {
        // Use tokio::fs::read to avoid blocking the async runtime thread.
        let pem = tokio::fs::read(bundle_path).await.map_err(|e| {
            Error::Network(format!("Failed to read CA bundle '{bundle_path}': {e}"))
        })?;
        let cert = reqwest::Certificate::from_pem(&pem)
            .map_err(|e| Error::Network(format!("Invalid CA bundle '{bundle_path}': {e}")))?;
        builder = builder.add_root_certificate(cert);
    }

    if let (Some(cert_path), Some(key_path)) = (client_cert, client_key) {
        let mut identity_pem = tokio::fs::read(cert_path).await.map_err(|e| {
            Error::Network(format!(
                "Failed to read client certificate '{cert_path}': {e}"
            ))
        })?;
        let key_pem = tokio::fs::read(key_path)
            .await
            .map_err(|e| Error::Network(format!("Failed to read client key '{key_path}': {e}")))?;
        identity_pem.extend_from_slice(b"\n");
        identity_pem.extend_from_slice(&key_pem);
        let identity = reqwest::Identity::from_pem(&identity_pem)
            .map_err(|e| Error::Network(format!("Invalid client certificate/key identity: {e}")))?;
        builder = builder.use_rustls_tls().identity(identity);
    }

    let client = builder
        .build()
        .map_err(|e| Error::Network(format!("Failed to build HTTP client: {e}")))?;
    Ok(client)
}

fn force_path_style_for_alias(alias: &Alias) -> bool {
    match alias.bucket_lookup.as_str() {
        "path" => true,
        "dns" => false,
        "auto" => !is_aliyun_oss_service_endpoint(&alias.endpoint),
        _ => true,
    }
}

fn sdk_retry_config(
    config: &rc_core::alias::RetryConfig,
) -> Result<aws_smithy_types::retry::RetryConfig> {
    if config.max_attempts == 0 {
        return Err(Error::Config(
            "Alias retry.max_attempts must be greater than zero".to_string(),
        ));
    }
    if config.initial_backoff_ms == 0
        || config.max_backoff_ms == 0
        || config.initial_backoff_ms > config.max_backoff_ms
    {
        return Err(Error::Config(
            "Alias retry backoff values must be non-zero and initial_backoff_ms must not exceed max_backoff_ms"
                .to_string(),
        ));
    }

    Ok(aws_smithy_types::retry::RetryConfigBuilder::new()
        .max_attempts(config.max_attempts)
        .initial_backoff(Duration::from_millis(config.initial_backoff_ms))
        .max_backoff(Duration::from_millis(config.max_backoff_ms))
        .build())
}

fn sdk_timeout_config(
    config: &rc_core::alias::TimeoutConfig,
) -> Result<aws_smithy_types::timeout::TimeoutConfig> {
    if config.connect_ms == 0 || config.read_ms == 0 {
        return Err(Error::Config(
            "Alias timeout values must be greater than zero".to_string(),
        ));
    }

    Ok(aws_smithy_types::timeout::TimeoutConfig::builder()
        .connect_timeout(Duration::from_millis(config.connect_ms))
        .read_timeout(Duration::from_millis(config.read_ms))
        .build())
}

fn is_aliyun_oss_service_endpoint(endpoint: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(endpoint.trim_end_matches('/')) else {
        return false;
    };

    let Some(host) = url.host_str() else {
        return false;
    };

    host.strip_suffix(".aliyuncs.com")
        .and_then(|host| host.split('.').next())
        .is_some_and(|first_label| first_label.starts_with("oss-"))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ReplicationConfigurationXml {
    role: Option<String>,
    #[serde(rename = "Rule", default)]
    rules: Vec<ReplicationRuleXml>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ReplicationRuleXml {
    #[serde(rename = "ID")]
    id: Option<String>,
    priority: Option<i32>,
    status: Option<String>,
    #[serde(rename = "Prefix")]
    legacy_prefix: Option<String>,
    filter: Option<ReplicationFilterXml>,
    destination: Option<ReplicationDestinationXml>,
    delete_marker_replication: Option<ReplicationStatusXml>,
    existing_object_replication: Option<ReplicationStatusXml>,
    delete_replication: Option<ReplicationStatusXml>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ReplicationFilterXml {
    prefix: Option<String>,
    tag: Option<TagXml>,
    and: Option<ReplicationAndXml>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ReplicationAndXml {
    prefix: Option<String>,
    #[serde(rename = "Tag", default)]
    tags: Vec<TagXml>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TagXml {
    key: Option<String>,
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ReplicationDestinationXml {
    bucket: Option<String>,
    storage_class: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ReplicationStatusXml {
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ReplicationResyncResponseDto {
    #[serde(default)]
    targets: Vec<ReplicationResyncTargetDto>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ReplicationResyncTargetDto {
    arn: String,
    #[serde(rename = "ResetID")]
    reset_id: String,
    #[serde(default)]
    reset_before_date: Option<String>,
    #[serde(default)]
    start_time: Option<String>,
    #[serde(default)]
    end_time: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    replicated_count: Option<i64>,
    #[serde(default)]
    replicated_size: Option<i64>,
    #[serde(default)]
    failed_count: Option<i64>,
    #[serde(default)]
    failed_size: Option<i64>,
    #[serde(default)]
    bucket: Option<String>,
    #[serde(default)]
    object: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct S3ExtensionErrorDto {
    code: String,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CorsConfigurationXml {
    #[serde(rename = "CORSRule", default)]
    rules: Vec<CorsRuleXml>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CorsRuleXml {
    #[serde(rename = "ID")]
    id: Option<String>,
    #[serde(rename = "AllowedOrigin", default)]
    allowed_origins: Vec<String>,
    #[serde(rename = "AllowedMethod", default)]
    allowed_methods: Vec<String>,
    #[serde(rename = "AllowedHeader", default)]
    allowed_headers: Vec<String>,
    #[serde(rename = "ExposeHeader", default)]
    expose_headers: Vec<String>,
    max_age_seconds: Option<i32>,
}

fn parse_replication_status(status: Option<&ReplicationStatusXml>) -> Option<bool> {
    status
        .and_then(|value| value.status.as_deref())
        .map(|value| value.eq_ignore_ascii_case("enabled"))
}

fn parse_replication_rule_status(status: Option<&str>) -> rc_core::ReplicationRuleStatus {
    match status {
        Some(value) if value.eq_ignore_ascii_case("enabled") => {
            rc_core::ReplicationRuleStatus::Enabled
        }
        _ => rc_core::ReplicationRuleStatus::Disabled,
    }
}

fn collect_tag_map<'a, I>(tags: I) -> Option<HashMap<String, String>>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let collected: HashMap<String, String> = tags
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    if collected.is_empty() {
        None
    } else {
        Some(collected)
    }
}

fn parse_tag_xml(tag: Option<&TagXml>) -> Option<HashMap<String, String>> {
    collect_tag_map(tag.and_then(|tag| Some((tag.key.as_deref()?, tag.value.as_deref()?))))
}

fn parse_tag_xmls(tags: &[TagXml]) -> Option<HashMap<String, String>> {
    collect_tag_map(
        tags.iter()
            .filter_map(|tag| Some((tag.key.as_deref()?, tag.value.as_deref()?))),
    )
}

fn parse_replication_filter_prefix(filter: Option<&ReplicationFilterXml>) -> Option<String> {
    filter
        .and_then(|filter| filter.prefix.clone())
        .or_else(|| filter.and_then(|filter| filter.and.as_ref()?.prefix.clone()))
}

fn parse_replication_filter_tags(
    filter: Option<&ReplicationFilterXml>,
) -> Option<HashMap<String, String>> {
    filter
        .and_then(|filter| parse_tag_xml(filter.tag.as_ref()))
        .or_else(|| filter.and_then(|filter| parse_tag_xmls(&filter.and.as_ref()?.tags)))
}

fn sorted_tags(tags: &HashMap<String, String>) -> Vec<(&str, &str)> {
    let mut pairs: Vec<(&str, &str)> = tags
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    pairs.sort_unstable();
    pairs
}

fn encode_object_tags(tags: &HashMap<String, String>) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in sorted_tags(tags) {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn append_tag_xml(xml: &mut String, key: &str, value: &str) {
    xml.push_str("<Tag><Key>");
    xml.push_str(&xml_escape(key));
    xml.push_str("</Key><Value>");
    xml.push_str(&xml_escape(value));
    xml.push_str("</Value></Tag>");
}

fn append_replication_filter_xml(
    xml: &mut String,
    prefix: Option<&str>,
    tags: Option<&HashMap<String, String>>,
) {
    let Some(tags) = tags.filter(|tags| !tags.is_empty()) else {
        if let Some(prefix) = prefix {
            xml.push_str("<Filter><Prefix>");
            xml.push_str(&xml_escape(prefix));
            xml.push_str("</Prefix></Filter>");
        }
        return;
    };

    xml.push_str("<Filter>");
    if prefix.is_some() || tags.len() > 1 {
        xml.push_str("<And>");
        if let Some(prefix) = prefix {
            xml.push_str("<Prefix>");
            xml.push_str(&xml_escape(prefix));
            xml.push_str("</Prefix>");
        }
        for (key, value) in sorted_tags(tags) {
            append_tag_xml(xml, key, value);
        }
        xml.push_str("</And>");
    } else if let Some((key, value)) = sorted_tags(tags).into_iter().next() {
        append_tag_xml(xml, key, value);
    }
    xml.push_str("</Filter>");
}

fn normalize_optional_strings(values: Option<Vec<String>>) -> Option<Vec<String>> {
    values.filter(|items| !items.is_empty())
}

fn is_missing_cors_configuration_error(error_text: &str) -> bool {
    let normalized = error_text.to_ascii_lowercase();
    normalized.contains("nosuchcorsconfiguration")
        || normalized.contains("cors configuration does not exist")
        || normalized.contains("the cors configuration does not exist")
}

fn is_missing_cors_configuration_response(
    error_code: Option<&str>,
    status_code: Option<u16>,
    error_text: &str,
) -> bool {
    let error_code = error_code.map(|code| code.to_ascii_lowercase());
    if matches!(error_code.as_deref(), Some("nosuchcorsconfiguration")) {
        return true;
    }

    if !is_missing_cors_configuration_error(error_text) {
        return false;
    }

    status_code.is_none_or(|status| status == 404)
}

fn sdk_cors_rule_to_core(rule: &aws_sdk_s3::types::CorsRule) -> CorsRule {
    CorsRule {
        id: rule.id().map(str::to_string),
        allowed_origins: rule.allowed_origins().to_vec(),
        allowed_methods: rule.allowed_methods().to_vec(),
        allowed_headers: normalize_optional_strings(Some(rule.allowed_headers().to_vec())),
        expose_headers: normalize_optional_strings(Some(rule.expose_headers().to_vec())),
        max_age_seconds: rule.max_age_seconds(),
    }
}

fn sdk_bucket_encryption_to_core(
    value: &aws_sdk_s3::types::ServerSideEncryptionByDefault,
) -> Result<BucketEncryption> {
    match value.sse_algorithm() {
        aws_sdk_s3::types::ServerSideEncryption::Aes256 => Ok(BucketEncryption::SseS3),
        aws_sdk_s3::types::ServerSideEncryption::AwsKms => Ok(BucketEncryption::SseKms {
            key_id: value.kms_master_key_id().map(ToString::to_string),
        }),
        other => Err(Error::General(format!(
            "unsupported bucket encryption algorithm: {}",
            other.as_str()
        ))),
    }
}

fn is_missing_bucket_encryption_error(error_text: &str) -> bool {
    let normalized = error_text.to_ascii_lowercase();
    normalized.contains("serversideencryptionconfigurationnotfounderror")
        || normalized.contains("nosuchbucketencryption")
        || normalized.contains("encryption configuration was not found")
}

fn is_missing_bucket_encryption_response(
    error_code: Option<&str>,
    status_code: Option<u16>,
    error_text: &str,
) -> bool {
    let error_code = error_code.map(|code| code.to_ascii_lowercase());
    if matches!(
        error_code.as_deref(),
        Some("serversideencryptionconfigurationnotfounderror" | "nosuchbucketencryption")
    ) {
        return true;
    }

    if !is_missing_bucket_encryption_error(error_text) {
        return false;
    }

    status_code.is_none_or(|status| status == 404)
}

fn core_bucket_encryption_to_sdk(
    value: &BucketEncryption,
) -> aws_sdk_s3::types::ServerSideEncryptionConfiguration {
    let encryption_by_default = match value {
        BucketEncryption::SseS3 => aws_sdk_s3::types::ServerSideEncryptionByDefault::builder()
            .sse_algorithm(aws_sdk_s3::types::ServerSideEncryption::Aes256)
            .build()
            .expect("sse-s3 bucket encryption configuration is valid"),
        BucketEncryption::SseKms { key_id } => {
            let mut builder = aws_sdk_s3::types::ServerSideEncryptionByDefault::builder()
                .sse_algorithm(aws_sdk_s3::types::ServerSideEncryption::AwsKms);
            if let Some(key_id) = key_id {
                builder = builder.kms_master_key_id(key_id);
            }
            builder
                .build()
                .expect("sse-kms bucket encryption configuration is valid")
        }
    };

    let rule = aws_sdk_s3::types::ServerSideEncryptionRule::builder()
        .apply_server_side_encryption_by_default(encryption_by_default)
        .build();

    aws_sdk_s3::types::ServerSideEncryptionConfiguration::builder()
        .rules(rule)
        .build()
        .expect("bucket encryption configuration requires one rule")
}

fn apply_object_encryption_to_put_request(
    request: aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder,
    encryption: Option<&ObjectEncryptionRequest>,
) -> aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder {
    match encryption {
        Some(ObjectEncryptionRequest::SseS3) => {
            request.server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::Aes256)
        }
        Some(ObjectEncryptionRequest::SseKms { key_id }) => request
            .server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::AwsKms)
            .ssekms_key_id(key_id),
        None => request,
    }
}

fn apply_object_encryption_to_copy_request(
    request: aws_sdk_s3::operation::copy_object::builders::CopyObjectFluentBuilder,
    encryption: Option<&ObjectEncryptionRequest>,
) -> aws_sdk_s3::operation::copy_object::builders::CopyObjectFluentBuilder {
    match encryption {
        Some(ObjectEncryptionRequest::SseS3) => {
            request.server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::Aes256)
        }
        Some(ObjectEncryptionRequest::SseKms { key_id }) => request
            .server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::AwsKms)
            .ssekms_key_id(key_id),
        None => request,
    }
}

fn apply_object_encryption_to_multipart_create_request(
    request: aws_sdk_s3::operation::create_multipart_upload::builders::CreateMultipartUploadFluentBuilder,
    encryption: Option<&ObjectEncryptionRequest>,
) -> aws_sdk_s3::operation::create_multipart_upload::builders::CreateMultipartUploadFluentBuilder {
    match encryption {
        Some(ObjectEncryptionRequest::SseS3) => {
            request.server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::Aes256)
        }
        Some(ObjectEncryptionRequest::SseKms { key_id }) => request
            .server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::AwsKms)
            .ssekms_key_id(key_id),
        None => request,
    }
}

struct SseCustomerHeaders {
    raw: Zeroizing<String>,
    key: Zeroizing<String>,
    key_md5: Zeroizing<String>,
}

impl SseCustomerHeaders {
    fn new(key: &SseCustomerKey) -> Self {
        let raw = Zeroizing::new(String::from_utf8_lossy(key.expose_secret()).into_owned());
        let encoded = Zeroizing::new(BASE64_STANDARD.encode(key.expose_secret()));
        let key_md5 = Zeroizing::new(BASE64_STANDARD.encode(Md5::digest(key.expose_secret())));
        Self {
            raw,
            key: encoded,
            key_md5,
        }
    }

    fn redaction_values(&self) -> [&str; 3] {
        [self.raw.as_str(), self.key.as_str(), self.key_md5.as_str()]
    }
}

fn apply_object_write_encryption_to_put_request(
    request: aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder,
    encryption: Option<&ObjectWriteEncryption>,
) -> aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder {
    match encryption {
        Some(ObjectWriteEncryption::Managed(encryption)) => {
            apply_object_encryption_to_put_request(request, Some(encryption))
        }
        Some(ObjectWriteEncryption::SseCustomer { key }) => {
            let headers = SseCustomerHeaders::new(key);
            request
                .sse_customer_algorithm("AES256")
                .sse_customer_key(headers.key.to_string())
                .sse_customer_key_md5(headers.key_md5.to_string())
        }
        None => request,
    }
}

fn apply_object_write_encryption_to_multipart_create_request(
    request: aws_sdk_s3::operation::create_multipart_upload::builders::CreateMultipartUploadFluentBuilder,
    encryption: Option<&ObjectWriteEncryption>,
) -> aws_sdk_s3::operation::create_multipart_upload::builders::CreateMultipartUploadFluentBuilder {
    match encryption {
        Some(ObjectWriteEncryption::Managed(encryption)) => {
            apply_object_encryption_to_multipart_create_request(request, Some(encryption))
        }
        Some(ObjectWriteEncryption::SseCustomer { key }) => {
            let headers = SseCustomerHeaders::new(key);
            request
                .sse_customer_algorithm("AES256")
                .sse_customer_key(headers.key.to_string())
                .sse_customer_key_md5(headers.key_md5.to_string())
        }
        None => request,
    }
}

fn sdk_legal_hold_status(status: LegalHoldStatus) -> aws_sdk_s3::types::ObjectLockLegalHoldStatus {
    match status {
        LegalHoldStatus::Off => aws_sdk_s3::types::ObjectLockLegalHoldStatus::Off,
        LegalHoldStatus::On => aws_sdk_s3::types::ObjectLockLegalHoldStatus::On,
    }
}

fn sdk_object_lock_mode(mode: RetentionMode) -> aws_sdk_s3::types::ObjectLockMode {
    match mode {
        RetentionMode::Governance => aws_sdk_s3::types::ObjectLockMode::Governance,
        RetentionMode::Compliance => aws_sdk_s3::types::ObjectLockMode::Compliance,
    }
}

fn apply_object_lock_to_put_request(
    mut request: aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder,
    options: &ObjectWriteOptions,
) -> Result<aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder> {
    if let Some(retention) = &options.retention {
        request = request
            .object_lock_mode(sdk_object_lock_mode(retention.mode))
            .object_lock_retain_until_date(sdk_timestamp(retention.retain_until)?);
    }
    if let Some(status) = options.legal_hold {
        request = request.object_lock_legal_hold_status(sdk_legal_hold_status(status));
    }
    Ok(request)
}

fn apply_object_lock_to_copy_request(
    mut request: aws_sdk_s3::operation::copy_object::builders::CopyObjectFluentBuilder,
    options: &ObjectWriteOptions,
) -> Result<aws_sdk_s3::operation::copy_object::builders::CopyObjectFluentBuilder> {
    if let Some(retention) = &options.retention {
        request = request
            .object_lock_mode(sdk_object_lock_mode(retention.mode))
            .object_lock_retain_until_date(sdk_timestamp(retention.retain_until)?);
    }
    if let Some(status) = options.legal_hold {
        request = request.object_lock_legal_hold_status(sdk_legal_hold_status(status));
    }
    Ok(request)
}

fn apply_object_lock_to_multipart_create_request(
    mut request: aws_sdk_s3::operation::create_multipart_upload::builders::CreateMultipartUploadFluentBuilder,
    options: &ObjectWriteOptions,
) -> Result<
    aws_sdk_s3::operation::create_multipart_upload::builders::CreateMultipartUploadFluentBuilder,
> {
    if let Some(retention) = &options.retention {
        request = request
            .object_lock_mode(sdk_object_lock_mode(retention.mode))
            .object_lock_retain_until_date(sdk_timestamp(retention.retain_until)?);
    }
    if let Some(status) = options.legal_hold {
        request = request.object_lock_legal_hold_status(sdk_legal_hold_status(status));
    }
    Ok(request)
}

fn apply_sse_customer_to_upload_part_request(
    request: aws_sdk_s3::operation::upload_part::builders::UploadPartFluentBuilder,
    key: Option<&SseCustomerKey>,
) -> aws_sdk_s3::operation::upload_part::builders::UploadPartFluentBuilder {
    let Some(key) = key else {
        return request;
    };
    let headers = SseCustomerHeaders::new(key);
    request
        .sse_customer_algorithm("AES256")
        .sse_customer_key(headers.key.to_string())
        .sse_customer_key_md5(headers.key_md5.to_string())
}

fn apply_sse_customer_to_head_request(
    request: aws_sdk_s3::operation::head_object::builders::HeadObjectFluentBuilder,
    key: Option<&SseCustomerKey>,
) -> aws_sdk_s3::operation::head_object::builders::HeadObjectFluentBuilder {
    let Some(key) = key else {
        return request;
    };
    let headers = SseCustomerHeaders::new(key);
    request
        .sse_customer_algorithm("AES256")
        .sse_customer_key(headers.key.to_string())
        .sse_customer_key_md5(headers.key_md5.to_string())
}

fn apply_sse_customer_to_get_request(
    request: aws_sdk_s3::operation::get_object::builders::GetObjectFluentBuilder,
    key: Option<&SseCustomerKey>,
) -> aws_sdk_s3::operation::get_object::builders::GetObjectFluentBuilder {
    let Some(key) = key else {
        return request;
    };
    let headers = SseCustomerHeaders::new(key);
    request
        .sse_customer_algorithm("AES256")
        .sse_customer_key(headers.key.to_string())
        .sse_customer_key_md5(headers.key_md5.to_string())
}

fn destination_sse_customer_key(
    encryption: Option<&ObjectWriteEncryption>,
) -> Option<&SseCustomerKey> {
    match encryption {
        Some(ObjectWriteEncryption::SseCustomer { key }) => Some(key),
        _ => None,
    }
}

fn apply_object_attributes_to_put_request(
    request: aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder,
    attributes: Option<&ObjectAttributes>,
) -> Result<aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder> {
    let Some(attributes) = attributes else {
        return Ok(request);
    };
    Ok(request
        .set_content_type(attributes.content_type.clone())
        .set_cache_control(attributes.cache_control.clone())
        .set_content_disposition(attributes.content_disposition.clone())
        .set_content_encoding(attributes.content_encoding.clone())
        .set_content_language(attributes.content_language.clone())
        .set_expires(attributes.expires.map(sdk_timestamp).transpose()?)
        .set_metadata(
            (!attributes.user_metadata.is_empty()).then(|| attributes.user_metadata.clone()),
        ))
}

fn apply_object_attributes_to_multipart_create_request(
    request: aws_sdk_s3::operation::create_multipart_upload::builders::CreateMultipartUploadFluentBuilder,
    attributes: &ObjectAttributes,
) -> Result<
    aws_sdk_s3::operation::create_multipart_upload::builders::CreateMultipartUploadFluentBuilder,
> {
    Ok(request
        .set_content_type(attributes.content_type.clone())
        .set_cache_control(attributes.cache_control.clone())
        .set_content_disposition(attributes.content_disposition.clone())
        .set_content_encoding(attributes.content_encoding.clone())
        .set_content_language(attributes.content_language.clone())
        .set_expires(attributes.expires.map(sdk_timestamp).transpose()?)
        .set_metadata(
            (!attributes.user_metadata.is_empty()).then(|| attributes.user_metadata.clone()),
        ))
}

fn managed_object_encryption(
    options: &ObjectWriteOptions,
) -> Result<Option<&ObjectEncryptionRequest>> {
    match options.encryption.as_ref() {
        Some(ObjectWriteEncryption::Managed(encryption)) => Ok(Some(encryption)),
        Some(ObjectWriteEncryption::SseCustomer { .. }) => Err(Error::UnsupportedFeature(
            "SSE-C writes are tracked by rustfs/backlog#1459".to_string(),
        )),
        None => Ok(None),
    }
}

fn rustfs_storage_class(value: Option<&str>) -> Result<Option<aws_sdk_s3::types::StorageClass>> {
    let Some(value) = value else {
        return Ok(None);
    };
    match value {
        "STANDARD" => Ok(Some(aws_sdk_s3::types::StorageClass::Standard)),
        "REDUCED_REDUNDANCY" => Ok(Some(aws_sdk_s3::types::StorageClass::ReducedRedundancy)),
        value if aws_sdk_s3::types::StorageClass::try_parse(value).is_ok() => {
            Err(Error::UnsupportedFeature(format!(
                "RustFS beta.10 does not provide meaningful storage policy '{value}'; tracked by rustfs/backlog#1465"
            )))
        }
        value => Err(Error::InvalidPath(format!(
            "Unknown destination storage class '{value}'"
        ))),
    }
}

fn sha256_checksum(data: &[u8]) -> String {
    BASE64_STANDARD.encode(Sha256::digest(data))
}

fn composite_sha256_checksum(part_digests: &[[u8; 32]]) -> String {
    let mut aggregate = Sha256::new();
    for digest in part_digests {
        aggregate.update(digest);
    }
    format!(
        "{}-{}",
        BASE64_STANDARD.encode(aggregate.finalize()),
        part_digests.len()
    )
}

fn requested_sha256_checksum(
    data: &[u8],
    request: Option<&ChecksumRequest>,
) -> Result<Option<String>> {
    match request {
        None => Ok(None),
        Some(ChecksumRequest::Calculate(ChecksumAlgorithm::Sha256)) => {
            Ok(Some(sha256_checksum(data)))
        }
        Some(ChecksumRequest::Precomputed(checksum))
            if checksum.algorithm == ChecksumAlgorithm::Sha256 =>
        {
            Ok(Some(checksum.value.clone()))
        }
        Some(_) => Err(Error::UnsupportedFeature(
            "RustFS beta.10 checksum writes currently support SHA-256 only".to_string(),
        )),
    }
}

fn validate_sha256_checksum_request(request: Option<&ChecksumRequest>) -> Result<()> {
    match request {
        None
        | Some(ChecksumRequest::Calculate(ChecksumAlgorithm::Sha256))
        | Some(ChecksumRequest::Precomputed(ObjectChecksum {
            algorithm: ChecksumAlgorithm::Sha256,
            ..
        })) => Ok(()),
        Some(_) => Err(Error::UnsupportedFeature(
            "RustFS beta.10 checksum writes currently support SHA-256 only".to_string(),
        )),
    }
}

fn persisted_sha256_checksum(
    value: &str,
    checksum_type: Option<&aws_sdk_s3::types::ChecksumType>,
) -> Result<ObjectChecksum> {
    let invalid =
        || Error::General("S3 returned an invalid persisted SHA-256 checksum".to_string());
    match checksum_type {
        Some(aws_sdk_s3::types::ChecksumType::FullObject) => {
            ObjectChecksum::new(ChecksumAlgorithm::Sha256, value).map_err(|_| invalid())
        }
        Some(aws_sdk_s3::types::ChecksumType::Composite) => {
            if ObjectChecksum::new(ChecksumAlgorithm::Sha256, value).is_ok() {
                return Err(invalid());
            }
            ObjectChecksum::new_persisted(ChecksumAlgorithm::Sha256, value).map_err(|_| invalid())
        }
        Some(value) => Err(Error::UnsupportedFeature(format!(
            "Unsupported S3 checksum type '{}'",
            value.as_str()
        ))),
        None => {
            ObjectChecksum::new_persisted(ChecksumAlgorithm::Sha256, value).map_err(|_| invalid())
        }
    }
}

fn validate_attribute_tag_write_options(options: &ObjectWriteOptions) -> Result<()> {
    options.validate()?;
    rustfs_storage_class(options.storage_class.as_deref())?;
    validate_sha256_checksum_request(options.checksum.as_ref())?;
    Ok(())
}

fn validate_beta10_copy_options(options: &TransferCopyOptions) -> Result<()> {
    options.validate()?;
    if options.source.checksum_mode {
        return Err(Error::UnsupportedFeature(
            "Checksum-mode copies are tracked by rustfs/backlog#1458".to_string(),
        ));
    }
    if options.source.customer_key.is_some() {
        return Err(Error::UnsupportedFeature(
            "RustFS beta.10 server-side copies from SSE-C sources are not compatibility-proven; tracked by rustfs/backlog#1467"
                .to_string(),
        ));
    }
    validate_attribute_tag_write_options(&options.destination)?;
    if matches!(
        options.destination.encryption,
        Some(ObjectWriteEncryption::SseCustomer { .. })
    ) {
        return Err(Error::UnsupportedFeature(
            "RustFS beta.10 server-side copies to SSE-C destinations are not compatibility-proven; tracked by rustfs/backlog#1467"
                .to_string(),
        ));
    }
    if options.destination.checksum.is_some() {
        return Err(Error::UnsupportedFeature(
            "RustFS beta.10 does not preserve CopyObject checksum selection; tracked by rustfs/backlog#1466"
                .to_string(),
        ));
    }
    if matches!(options.metadata_directive, Some(MetadataDirective::Replace)) {
        return Err(Error::UnsupportedFeature(
            "RustFS beta.10 does not preserve complete metadata REPLACE semantics; tracked by rustfs/backlog#1463"
                .to_string(),
        ));
    }
    if options.tagging_directive.is_some() || options.destination.tags.is_some() {
        return Err(Error::UnsupportedFeature(
            "RustFS beta.10 does not preserve CopyObject tagging directives; tracked by rustfs/backlog#1462"
                .to_string(),
        ));
    }
    Ok(())
}

fn encoded_copy_source(src: &RemotePath, source_version_id: Option<&str>) -> String {
    let encoded_key = src
        .key
        .split('/')
        .map(|segment| urlencoding::encode(segment).into_owned())
        .collect::<Vec<_>>()
        .join("/");
    let mut copy_source = format!("{}/{encoded_key}", urlencoding::encode(&src.bucket));

    if let Some(version_id) = source_version_id {
        copy_source.push_str("?versionId=");
        copy_source.push_str(&urlencoding::encode(version_id));
    }

    copy_source
}

fn quoted_etag(etag: &str) -> String {
    format!("\"{}\"", etag.trim_matches('"'))
}

fn sdk_retention_mode(mode: RetentionMode) -> aws_sdk_s3::types::ObjectLockRetentionMode {
    match mode {
        RetentionMode::Governance => aws_sdk_s3::types::ObjectLockRetentionMode::Governance,
        RetentionMode::Compliance => aws_sdk_s3::types::ObjectLockRetentionMode::Compliance,
    }
}

fn core_retention_mode(mode: &aws_sdk_s3::types::ObjectLockRetentionMode) -> Result<RetentionMode> {
    match mode.as_str() {
        "GOVERNANCE" => Ok(RetentionMode::Governance),
        "COMPLIANCE" => Ok(RetentionMode::Compliance),
        value => Err(Error::General(format!(
            "Unsupported Object Lock retention mode '{value}'"
        ))),
    }
}

fn sdk_default_retention(default: DefaultRetention) -> Result<aws_sdk_s3::types::DefaultRetention> {
    if default.duration.value <= 0 {
        return Err(Error::InvalidPath(
            "Retention duration must be a positive number of days or years".to_string(),
        ));
    }
    let builder =
        aws_sdk_s3::types::DefaultRetention::builder().mode(sdk_retention_mode(default.mode));
    Ok(match default.duration.unit {
        RetentionDurationUnit::Days => builder.days(default.duration.value).build(),
        RetentionDurationUnit::Years => builder.years(default.duration.value).build(),
    })
}

fn core_retention_duration(
    retention: &aws_sdk_s3::types::DefaultRetention,
) -> Result<RetentionDuration> {
    match (retention.days(), retention.years()) {
        (Some(days), None) => RetentionDuration::days(days).map_err(|_| {
            Error::General(format!(
                "Bucket Object Lock configuration contains an invalid Days value: {days}"
            ))
        }),
        (None, Some(years)) => RetentionDuration::years(years).map_err(|_| {
            Error::General(format!(
                "Bucket Object Lock configuration contains an invalid Years value: {years}"
            ))
        }),
        (Some(_), Some(_)) => Err(Error::General(
            "Bucket Object Lock configuration contains both Days and Years".to_string(),
        )),
        (None, None) => Err(Error::General(
            "Bucket Object Lock configuration is missing its retention duration".to_string(),
        )),
    }
}

fn sdk_timestamp(timestamp: Timestamp) -> Result<aws_smithy_types::DateTime> {
    let nanoseconds = u32::try_from(timestamp.subsec_nanosecond()).map_err(|error| {
        Error::General(format!(
            "Object retention timestamp has invalid nanoseconds: {error}"
        ))
    })?;
    Ok(aws_smithy_types::DateTime::from_secs_and_nanos(
        timestamp.as_second(),
        nanoseconds,
    ))
}

fn core_timestamp(timestamp: &aws_smithy_types::DateTime) -> Result<Timestamp> {
    let nanoseconds = i32::try_from(timestamp.subsec_nanos()).map_err(|error| {
        Error::General(format!(
            "Object retention timestamp has invalid nanoseconds: {error}"
        ))
    })?;
    Timestamp::new(timestamp.secs(), nanoseconds).map_err(|error| {
        Error::General(format!(
            "Object retention timestamp is outside the supported range: {error}"
        ))
    })
}

fn core_http_date(value: &str) -> Result<Timestamp> {
    let timestamp =
        aws_smithy_types::DateTime::from_str(value, aws_smithy_types::date_time::Format::HttpDate)
            .map_err(|error| {
                Error::General(format!("Object expiry is not a valid HTTP date: {error}"))
            })?;
    core_timestamp(&timestamp)
}

fn sdk_bucket_lock_configuration(
    configuration: BucketObjectLockConfiguration,
) -> Result<aws_sdk_s3::types::ObjectLockConfiguration> {
    if !configuration.enabled {
        return Err(Error::InvalidPath(
            "Object Lock cannot be disabled after it has been enabled".to_string(),
        ));
    }

    let mut builder = aws_sdk_s3::types::ObjectLockConfiguration::builder()
        .object_lock_enabled(aws_sdk_s3::types::ObjectLockEnabled::Enabled);
    if let Some(default) = configuration.default_retention {
        let retention = sdk_default_retention(default)?;
        let rule = aws_sdk_s3::types::ObjectLockRule::builder()
            .default_retention(retention)
            .build();
        builder = builder.rule(rule);
    }
    Ok(builder.build())
}

fn core_bucket_lock_configuration(
    configuration: &aws_sdk_s3::types::ObjectLockConfiguration,
) -> Result<BucketObjectLockConfiguration> {
    let enabled = match configuration
        .object_lock_enabled()
        .map(|value| value.as_str())
    {
        Some("Enabled") => true,
        None => false,
        Some(value) => {
            return Err(Error::General(format!(
                "Unsupported bucket Object Lock enabled state '{value}'"
            )));
        }
    };
    let default_retention = configuration
        .rule()
        .and_then(|rule| rule.default_retention())
        .map(|retention| -> Result<DefaultRetention> {
            let mode = retention.mode().ok_or_else(|| {
                Error::General(
                    "Bucket Object Lock configuration is missing its retention mode".to_string(),
                )
            })?;
            Ok(DefaultRetention {
                mode: core_retention_mode(mode)?,
                duration: core_retention_duration(retention)?,
            })
        })
        .transpose()?;

    Ok(BucketObjectLockConfiguration {
        enabled,
        default_retention,
    })
}

fn core_cors_rule_to_sdk(rule: &CorsRule) -> Result<aws_sdk_s3::types::CorsRule> {
    aws_sdk_s3::types::CorsRule::builder()
        .set_id(rule.id.clone())
        .set_allowed_origins(Some(rule.allowed_origins.clone()))
        .set_allowed_methods(Some(
            rule.allowed_methods
                .iter()
                .map(|method| method.to_ascii_uppercase())
                .collect(),
        ))
        .set_allowed_headers(normalize_optional_strings(rule.allowed_headers.clone()))
        .set_expose_headers(normalize_optional_strings(rule.expose_headers.clone()))
        .set_max_age_seconds(rule.max_age_seconds)
        .build()
        .map_err(|e| Error::General(format!("build bucket cors rule: {e}")))
}

fn parse_cors_configuration_xml(body: &str) -> Result<Vec<CorsRule>> {
    let config: CorsConfigurationXml =
        from_xml_str(body).map_err(|e| Error::General(format!("parse bucket cors xml: {e}")))?;

    Ok(config
        .rules
        .into_iter()
        .map(|rule| CorsRule {
            id: rule.id,
            allowed_origins: rule.allowed_origins,
            allowed_methods: rule.allowed_methods,
            allowed_headers: normalize_optional_strings(Some(rule.allowed_headers)),
            expose_headers: normalize_optional_strings(Some(rule.expose_headers)),
            max_age_seconds: rule.max_age_seconds,
        })
        .collect())
}

fn parse_replication_configuration_xml(body: &str) -> Result<ReplicationConfiguration> {
    let config: ReplicationConfigurationXml = from_xml_str(body)
        .map_err(|e| Error::General(format!("parse replication config xml: {e}")))?;

    let rules = config
        .rules
        .into_iter()
        .map(|rule| rc_core::ReplicationRule {
            id: rule.id.unwrap_or_default(),
            priority: rule.priority.unwrap_or_default(),
            status: parse_replication_rule_status(rule.status.as_deref()),
            prefix: parse_replication_filter_prefix(rule.filter.as_ref()).or(rule.legacy_prefix),
            tags: parse_replication_filter_tags(rule.filter.as_ref()),
            destination: rc_core::ReplicationDestination {
                bucket_arn: rule
                    .destination
                    .as_ref()
                    .and_then(|destination| destination.bucket.clone())
                    .unwrap_or_default(),
                storage_class: rule
                    .destination
                    .and_then(|destination| destination.storage_class),
            },
            delete_marker_replication: parse_replication_status(
                rule.delete_marker_replication.as_ref(),
            ),
            existing_object_replication: parse_replication_status(
                rule.existing_object_replication.as_ref(),
            ),
            delete_replication: parse_replication_status(rule.delete_replication.as_ref()),
        })
        .collect();

    Ok(ReplicationConfiguration {
        role: config.role.unwrap_or_default(),
        rules,
    })
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn append_replication_status_tag(xml: &mut String, tag: &str, enabled: Option<bool>) {
    if let Some(enabled) = enabled {
        let status = if enabled { "Enabled" } else { "Disabled" };
        xml.push('<');
        xml.push_str(tag);
        xml.push_str("><Status>");
        xml.push_str(status);
        xml.push_str("</Status></");
        xml.push_str(tag);
        xml.push('>');
    }
}

fn build_replication_configuration_xml(config: &ReplicationConfiguration) -> String {
    let mut xml = String::from(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    xml.push_str(r#"<ReplicationConfiguration xmlns=""#);
    xml.push_str(S3_REPLICATION_XML_NAMESPACE);
    xml.push_str(r#"">"#);

    if !config.role.is_empty() {
        xml.push_str("<Role>");
        xml.push_str(&xml_escape(&config.role));
        xml.push_str("</Role>");
    }

    for rule in &config.rules {
        xml.push_str("<Rule>");

        xml.push_str("<Status>");
        xml.push_str(match rule.status {
            rc_core::ReplicationRuleStatus::Enabled => "Enabled",
            rc_core::ReplicationRuleStatus::Disabled => "Disabled",
        });
        xml.push_str("</Status>");

        xml.push_str("<Destination><Bucket>");
        xml.push_str(&xml_escape(&rule.destination.bucket_arn));
        xml.push_str("</Bucket>");
        if let Some(storage_class) = &rule.destination.storage_class {
            xml.push_str("<StorageClass>");
            xml.push_str(&xml_escape(storage_class));
            xml.push_str("</StorageClass>");
        }
        xml.push_str("</Destination>");

        if !rule.id.is_empty() {
            xml.push_str("<ID>");
            xml.push_str(&xml_escape(&rule.id));
            xml.push_str("</ID>");
        }

        xml.push_str("<Priority>");
        xml.push_str(&rule.priority.to_string());
        xml.push_str("</Priority>");

        append_replication_filter_xml(&mut xml, rule.prefix.as_deref(), rule.tags.as_ref());

        append_replication_status_tag(
            &mut xml,
            "ExistingObjectReplication",
            rule.existing_object_replication,
        );
        append_replication_status_tag(
            &mut xml,
            "DeleteMarkerReplication",
            rule.delete_marker_replication,
        );
        append_replication_status_tag(&mut xml, "DeleteReplication", rule.delete_replication);

        xml.push_str("</Rule>");
    }

    xml.push_str("</ReplicationConfiguration>");
    xml
}

fn parse_lifecycle_filter_prefix(
    filter: Option<&aws_sdk_s3::types::LifecycleRuleFilter>,
) -> Option<String> {
    filter
        .and_then(|filter| filter.prefix().map(str::to_string))
        .or_else(|| filter.and_then(|filter| filter.and()?.prefix().map(str::to_string)))
}

fn parse_lifecycle_filter_tags(
    filter: Option<&aws_sdk_s3::types::LifecycleRuleFilter>,
) -> Option<HashMap<String, String>> {
    filter
        .and_then(|filter| collect_tag_map(filter.tag().map(|tag| (tag.key(), tag.value()))))
        .or_else(|| {
            filter.and_then(|filter| {
                collect_tag_map(
                    filter
                        .and()?
                        .tags()
                        .iter()
                        .map(|tag| (tag.key(), tag.value())),
                )
            })
        })
}

fn build_s3_tag(key: &str, value: &str) -> Result<aws_sdk_s3::types::Tag> {
    aws_sdk_s3::types::Tag::builder()
        .key(key)
        .value(value)
        .build()
        .map_err(|error| Error::General(format!("build filter tag: {error}")))
}

fn build_lifecycle_rule_filter(
    prefix: Option<&str>,
    tags: Option<&HashMap<String, String>>,
) -> Result<Option<aws_sdk_s3::types::LifecycleRuleFilter>> {
    let Some(tags) = tags.filter(|tags| !tags.is_empty()) else {
        return Ok(prefix.map(|prefix| {
            aws_sdk_s3::types::LifecycleRuleFilter::builder()
                .prefix(prefix)
                .build()
        }));
    };

    let tag_values = sorted_tags(tags)
        .into_iter()
        .map(|(key, value)| build_s3_tag(key, value))
        .collect::<Result<Vec<_>>>()?;

    let filter = if prefix.is_some() || tag_values.len() > 1 {
        let mut and_builder = aws_sdk_s3::types::LifecycleRuleAndOperator::builder();
        if let Some(prefix) = prefix {
            and_builder = and_builder.prefix(prefix);
        }
        for tag in tag_values {
            and_builder = and_builder.tags(tag);
        }
        aws_sdk_s3::types::LifecycleRuleFilter::builder()
            .and(and_builder.build())
            .build()
    } else {
        aws_sdk_s3::types::LifecycleRuleFilter::builder()
            .tag(
                tag_values
                    .into_iter()
                    .next()
                    .expect("non-empty tags required to build lifecycle filter"),
            )
            .build()
    };

    Ok(Some(filter))
}

fn validate_lifecycle_rule(rule: &LifecycleRule) -> Result<()> {
    if rule.expired_object_delete_marker != Some(true) {
        return Ok(());
    }

    if rule
        .expiration
        .as_ref()
        .is_some_and(|expiration| expiration.days.is_some() || expiration.date.is_some())
    {
        return Err(Error::InvalidPath(format!(
            "lifecycle rule '{}' cannot combine current expiration days or date with expired delete-marker cleanup",
            rule.id
        )));
    }

    if rule.tags.as_ref().is_some_and(|tags| !tags.is_empty()) {
        return Err(Error::InvalidPath(format!(
            "lifecycle rule '{}' cannot combine tag filters with expired delete-marker cleanup",
            rule.id
        )));
    }

    Ok(())
}

impl HttpConnector for ReqwestConnector {
    fn call(&self, mut request: HttpRequest) -> HttpConnectorFuture {
        let client = self.client.clone();
        HttpConnectorFuture::new(async move {
            // Extract request parts before consuming the request
            let uri = request.uri().to_string();
            let method_str = request.method().to_string();
            let headers = request.headers().clone();

            let body = match request.body().bytes() {
                Some(bytes) => reqwest::Body::from(Bytes::copy_from_slice(bytes)),
                None => reqwest::Body::wrap_stream(http_body_util::BodyDataStream::new(
                    request.take_body(),
                )),
            };

            // Build reqwest method
            let method = reqwest::Method::from_bytes(method_str.as_bytes())
                .map_err(|e| ConnectorError::user(Box::new(e)))?;

            // Build reqwest URL
            let url = reqwest::Url::parse(&uri).map_err(|e| ConnectorError::user(Box::new(e)))?;

            // Build reqwest request
            let mut req = reqwest::Request::new(method, url);

            // Copy headers; S3 headers are all ASCII so failures here are unexpected
            for (name, value) in headers.iter() {
                match (
                    reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                    reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
                ) {
                    (Ok(header_name), Ok(header_value)) => {
                        req.headers_mut().append(header_name, header_value);
                    }
                    _ => {
                        tracing::warn!("Skipping non-convertible request header: {}", name);
                    }
                }
            }

            // Set body
            *req.body_mut() = Some(body);

            // Execute
            let resp = client
                .execute(req)
                .await
                .map_err(|e| ConnectorError::io(Box::new(e)))?;

            // Convert response
            let status = StatusCode::try_from(resp.status().as_u16())
                .map_err(|e| ConnectorError::other(Box::new(e), None))?;
            let resp_headers = resp.headers().clone();
            let body = StreamBody::new(resp.bytes_stream().map_ok(Frame::data));
            let mut sdk_response = Response::new(status, SdkBody::from_body_1_x(body));
            for (name, value) in &resp_headers {
                match value.to_str() {
                    Ok(value_str) => {
                        sdk_response
                            .headers_mut()
                            .append(name.as_str().to_owned(), value_str.to_owned());
                    }
                    Err(_) => {
                        tracing::warn!("Skipping non-UTF8 response header: {}", name.as_str());
                    }
                }
            }

            Ok(sdk_response)
        })
    }
}

impl HttpClient for ReqwestConnector {
    fn http_connector(
        &self,
        _settings: &HttpConnectorSettings,
        _components: &RuntimeComponents,
    ) -> SharedHttpConnector {
        // NOTE: `ReqwestConnector` is preconfigured (e.g., insecure/CA-bundle options) when it
        // is constructed, and does not currently apply `HttpConnectorSettings`. This means
        // behavior in this mode may differ from the default connector w.r.t. SDK HTTP settings.
        // If alignment is required, map relevant fields from `HttpConnectorSettings` onto the
        // internal `reqwest::Client` when constructing the connector.
        SharedHttpConnector::new(self.clone())
    }
}

/// S3 client wrapper
pub struct S3Client {
    inner: aws_sdk_s3::Client,
    presign_inner: aws_sdk_s3::Client,
    xml_http_client: reqwest::Client,
    alias: Alias,
    request_headers: Vec<RequestHeader>,
}

#[derive(Debug, Clone)]
struct CustomHeaderInterceptor {
    headers: Vec<RequestHeader>,
}

impl Intercept for CustomHeaderInterceptor {
    fn name(&self) -> &'static str {
        "CustomHeaderInterceptor"
    }

    fn modify_before_signing(
        &self,
        context: &mut BeforeTransmitInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        _cfg: &mut ConfigBag,
    ) -> std::result::Result<(), BoxError> {
        let request = context.request_mut();
        for header in &self.headers {
            request
                .headers_mut()
                .try_insert(header.name.clone(), header.value.clone())
                .map_err(|error| Box::new(error) as BoxError)?;
        }
        Ok(())
    }
}

impl S3Client {
    /// Create a new S3 client from an alias configuration
    pub async fn new(alias: Alias) -> Result<Self> {
        let endpoint = alias.endpoint.clone();
        let region = alias.region.clone();
        let access_key = alias.access_key.clone();
        let secret_key = alias.secret_key.clone();

        // Build SDK config loader
        let mut config_loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region))
            .endpoint_url(&endpoint);

        if alias.anonymous {
            config_loader = config_loader.no_credentials();
        } else {
            let credentials = aws_credential_types::Credentials::new(
                access_key,
                secret_key,
                None, // session token
                None, // expiry
                "rc-static-credentials",
            );
            config_loader = config_loader.credentials_provider(credentials);
        }

        if let Some(retry) = &alias.retry {
            config_loader = config_loader.retry_config(sdk_retry_config(retry)?);
        }
        if let Some(timeout) = &alias.timeout {
            config_loader = config_loader.timeout_config(sdk_timeout_config(timeout)?);
        }

        // When insecure mode is enabled or a custom CA bundle is provided, use the reqwest
        // connector which supports danger_accept_invalid_certs and custom root certificates.
        if alias.insecure
            || alias.ca_bundle.is_some()
            || (alias.client_cert.is_some() && alias.client_key.is_some())
        {
            let connector = ReqwestConnector::new(
                alias.insecure,
                alias.ca_bundle.as_deref(),
                alias.client_cert.as_deref(),
                alias.client_key.as_deref(),
                alias.timeout.as_ref(),
            )
            .await?;
            config_loader = config_loader.http_client(connector);
        }

        let xml_http_client = build_reqwest_client(
            alias.insecure,
            alias.ca_bundle.as_deref(),
            alias.client_cert.as_deref(),
            alias.client_key.as_deref(),
            alias.timeout.as_ref(),
        )
        .await?;
        let config = config_loader.load().await;

        // Build S3 client with path-style addressing for compatibility
        let request_headers = global_request_headers();
        let mut s3_config_builder = aws_sdk_s3::config::Builder::from(&config)
            .force_path_style(force_path_style_for_alias(&alias))
            // Improve compatibility with S3-compatible backends by only sending request
            // checksums when the operation explicitly requires them.
            .request_checksum_calculation(
                aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
            )
            .response_checksum_validation(
                aws_sdk_s3::config::ResponseChecksumValidation::WhenRequired,
            );

        let presign_s3_config = s3_config_builder.clone().build();

        if !request_headers.is_empty() {
            s3_config_builder = s3_config_builder.interceptor(CustomHeaderInterceptor {
                headers: request_headers.clone(),
            });
        }

        let s3_config = s3_config_builder.build();

        let client = aws_sdk_s3::Client::from_conf(s3_config);
        let presign_client = aws_sdk_s3::Client::from_conf(presign_s3_config);

        Ok(Self {
            inner: client,
            presign_inner: presign_client,
            xml_http_client,
            alias,
            request_headers,
        })
    }

    fn ensure_sse_customer_transport(&self, key: Option<&SseCustomerKey>) -> Result<()> {
        if key.is_none() {
            return Ok(());
        }
        if self.alias.insecure
            || !self
                .alias
                .endpoint
                .to_ascii_lowercase()
                .starts_with("https://")
        {
            return Err(Error::UnsupportedFeature(
                "SSE-C requires an HTTPS endpoint with certificate verification enabled"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn ensure_write_sse_customer_transport(
        &self,
        encryption: Option<&ObjectWriteEncryption>,
    ) -> Result<()> {
        self.ensure_sse_customer_transport(destination_sse_customer_key(encryption))
    }

    /// Get the underlying aws-sdk-s3 client
    pub fn inner(&self) -> &aws_sdk_s3::Client {
        &self.inner
    }

    pub(crate) fn watch_alias(&self) -> &Alias {
        &self.alias
    }

    pub(crate) fn watch_http_client(&self) -> &reqwest::Client {
        &self.xml_http_client
    }

    pub(crate) fn watch_request_headers(&self) -> &[RequestHeader] {
        &self.request_headers
    }

    pub(crate) fn watch_request_host(&self, url: &reqwest::Url) -> Result<String> {
        self.request_host(url)
    }

    pub(crate) async fn sign_watch_request(
        &self,
        method: &Method,
        url: &str,
        headers: &HeaderMap,
    ) -> Result<HeaderMap> {
        self.sign_xml_request(method, url, headers, &[]).await
    }

    /// List a single page of object versions and return pagination metadata.
    pub async fn list_object_versions_page(
        &self,
        path: &RemotePath,
        max_keys: Option<i32>,
    ) -> Result<ObjectVersionListResult> {
        self.list_object_versions_page_with_markers(path, max_keys, None, None)
            .await
    }

    pub async fn list_object_versions_page_with_markers(
        &self,
        path: &RemotePath,
        max_keys: Option<i32>,
        key_marker: Option<&str>,
        version_id_marker: Option<&str>,
    ) -> Result<ObjectVersionListResult> {
        let mut builder = self.inner.list_object_versions().bucket(&path.bucket);

        if !path.key.is_empty() {
            builder = builder.prefix(&path.key);
        }

        if let Some(max) = max_keys {
            builder = builder.max_keys(max);
        }
        if let Some(key_marker) = key_marker {
            builder = builder.key_marker(key_marker);
        }
        if let Some(version_id_marker) = version_id_marker {
            builder = builder.version_id_marker(version_id_marker);
        }

        let response = builder.send().await.map_err(|error| {
            let formatted = Self::format_sdk_error(&error);
            if let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &error {
                let status = service_error.raw().status().as_u16();
                let code = service_error.err().code();
                if matches!(status, 401 | 403)
                    || matches!(
                        code,
                        Some("AccessDenied") | Some("Forbidden") | Some("Unauthorized")
                    )
                {
                    return Error::Auth(formatted);
                }
                if status == 404 || matches!(code, Some("NotFound") | Some("NoSuchBucket")) {
                    return Error::NotFound(format!("Bucket not found: {}", path.bucket));
                }
            }
            if formatted.contains("AccessDenied")
                || formatted.contains("Forbidden")
                || formatted.contains("Unauthorized")
            {
                Error::Auth(formatted)
            } else if formatted.contains("NotFound") || formatted.contains("NoSuchBucket") {
                Error::NotFound(format!("Bucket not found: {}", path.bucket))
            } else {
                Error::Network(formatted)
            }
        })?;

        let mut items = Vec::new();

        for v in response.versions() {
            items.push(ObjectVersion {
                key: v.key().unwrap_or_default().to_string(),
                version_id: v.version_id().unwrap_or("null").to_string(),
                is_latest: v.is_latest().unwrap_or(false),
                is_delete_marker: false,
                last_modified: v
                    .last_modified()
                    .and_then(|timestamp| core_timestamp(timestamp).ok()),
                size_bytes: v.size(),
                etag: v.e_tag().map(|s| s.trim_matches('"').to_string()),
            });
        }

        for m in response.delete_markers() {
            items.push(ObjectVersion {
                key: m.key().unwrap_or_default().to_string(),
                version_id: m.version_id().unwrap_or("null").to_string(),
                is_latest: m.is_latest().unwrap_or(false),
                is_delete_marker: true,
                last_modified: m
                    .last_modified()
                    .and_then(|timestamp| core_timestamp(timestamp).ok()),
                size_bytes: None,
                etag: None,
            });
        }

        items.sort_by(|a, b| {
            a.key
                .cmp(&b.key)
                .then_with(|| b.last_modified.cmp(&a.last_modified))
        });

        Ok(ObjectVersionListResult {
            items,
            truncated: response.is_truncated().unwrap_or(false),
            continuation_token: response.next_key_marker().map(ToString::to_string),
            version_id_marker: response.next_version_id_marker().map(ToString::to_string),
        })
    }

    /// Download object content and report downloaded bytes after each received chunk.
    pub async fn get_object_with_progress(
        &self,
        path: &RemotePath,
        on_progress: impl FnMut(u64, Option<u64>) + Send,
    ) -> Result<Vec<u8>> {
        self.get_object_with_progress_and_options(path, &ObjectReadOptions::default(), on_progress)
            .await
    }

    /// Download an exact object version and report downloaded bytes after each chunk.
    pub async fn get_object_with_progress_and_options(
        &self,
        path: &RemotePath,
        options: &ObjectReadOptions,
        mut on_progress: impl FnMut(u64, Option<u64>) + Send,
    ) -> Result<Vec<u8>> {
        let mut request = self.inner.get_object().bucket(&path.bucket).key(&path.key);
        if let Some(version_id) = &options.version_id {
            request = request.version_id(version_id);
        }
        let response = request.send().await.map_err(|error| {
            Self::map_object_request_error(&error, path, options.version_id.as_deref())
        })?;

        if response.delete_marker().unwrap_or(false) {
            return Err(Error::DeleteMarker {
                path: path.to_string(),
                version_id: response
                    .version_id()
                    .or(options.version_id.as_deref())
                    .unwrap_or("unknown")
                    .to_string(),
            });
        }

        let content_length = response
            .content_length()
            .and_then(|length| u64::try_from(length).ok());
        let initial_capacity = content_length.unwrap_or_default().min(8 * 1024 * 1024);
        let mut data = Vec::with_capacity(usize::try_from(initial_capacity).unwrap_or_default());
        let mut body = response.body;
        let mut bytes_downloaded = 0u64;

        while let Some(chunk) = body
            .try_next()
            .await
            .map_err(|e| Error::Network(e.to_string()))?
        {
            bytes_downloaded += chunk.len() as u64;
            data.extend_from_slice(&chunk);
            on_progress(bytes_downloaded, content_length);
        }

        Ok(data)
    }

    pub async fn download_object_to_path(
        &self,
        path: &RemotePath,
        destination: &std::path::Path,
        on_progress: impl FnMut(u64, Option<u64>) + Send,
    ) -> Result<u64> {
        self.download_object_to_path_with_transfer_options(
            path,
            destination,
            &TransferReadOptions::default(),
            on_progress,
        )
        .await
    }

    /// Download an object to a local path with version and SSE-C source options.
    pub async fn download_object_to_path_with_transfer_options(
        &self,
        path: &RemotePath,
        destination: &std::path::Path,
        options: &TransferReadOptions,
        mut on_progress: impl FnMut(u64, Option<u64>) + Send,
    ) -> Result<u64> {
        options.validate()?;
        self.ensure_sse_customer_transport(options.customer_key.as_ref())?;
        let mut request = apply_sse_customer_to_get_request(
            self.inner.get_object().bucket(&path.bucket).key(&path.key),
            options.customer_key.as_ref(),
        );
        if let Some(version_id) = &options.version_id {
            request = request.version_id(version_id);
        }
        if options.checksum_mode {
            request = request.checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled);
        }
        let response = request.send().await.map_err(|error| {
            self.redact_sse_customer_error(
                Self::map_object_request_error(&error, path, options.version_id.as_deref()),
                options.customer_key.as_ref(),
            )
        })?;
        let content_length = response
            .content_length()
            .and_then(|length| u64::try_from(length).ok());
        let parent = destination.parent().ok_or_else(|| {
            Error::General(format!(
                "download destination '{}' has no parent directory",
                destination.display()
            ))
        })?;
        let file_name = destination.file_name().ok_or_else(|| {
            Error::General(format!(
                "download destination '{}' has no file name",
                destination.display()
            ))
        })?;
        let temporary_file = tempfile::Builder::new()
            .prefix(&format!(".{}.rc-part-", file_name.to_string_lossy()))
            .tempfile_in(parent)
            .map_err(|error| {
                Error::General(format!(
                    "create temporary download in '{}': {error}",
                    parent.display()
                ))
            })?;
        let (file, temporary) = temporary_file.into_parts();
        let mut file = tokio::fs::File::from_std(file);
        let mut body = response.body;
        let mut bytes_downloaded = 0u64;

        while let Some(chunk) = match body.try_next().await {
            Ok(chunk) => chunk,
            Err(error) => {
                return Err(self.redact_sse_customer_error(
                    Error::Network(error.to_string()),
                    options.customer_key.as_ref(),
                ));
            }
        } {
            if let Err(error) = file.write_all(&chunk).await {
                return Err(Error::General(format!(
                    "write download destination '{}': {error}",
                    destination.display()
                )));
            }
            bytes_downloaded += chunk.len() as u64;
            on_progress(bytes_downloaded, content_length);
        }

        if let Err(error) = file.flush().await {
            return Err(Error::General(format!(
                "flush download destination '{}': {error}",
                destination.display()
            )));
        }

        drop(file);
        temporary.persist(destination).map_err(|error| {
            Error::General(format!(
                "atomically replace download destination '{}': {}",
                destination.display(),
                error.error
            ))
        })?;

        Ok(bytes_downloaded)
    }

    pub async fn write_object_to<W>(
        &self,
        path: &RemotePath,
        writer: &mut W,
        max_bytes: Option<u64>,
    ) -> Result<u64>
    where
        W: AsyncWrite + Unpin + Send + ?Sized,
    {
        self.write_object_to_with_options(path, &ObjectReadOptions::default(), writer, max_bytes)
            .await
    }

    /// Stream the current object or an exact historical version to a writer.
    pub async fn write_object_to_with_options<W>(
        &self,
        path: &RemotePath,
        options: &ObjectReadOptions,
        writer: &mut W,
        max_bytes: Option<u64>,
    ) -> Result<u64>
    where
        W: AsyncWrite + Unpin + Send + ?Sized,
    {
        if matches!(max_bytes, Some(0)) {
            if options.version_id.is_some() {
                self.head_object_with_options(path, options).await?;
            }
            return Ok(0);
        }

        let mut request = self.inner.get_object().bucket(&path.bucket).key(&path.key);
        if let Some(version_id) = &options.version_id {
            request = request.version_id(version_id);
        }
        if let Some(max_bytes) = max_bytes {
            request = request.range(format!("bytes=0-{}", max_bytes - 1));
        }
        let response = request.send().await.map_err(|error| {
            Self::map_object_request_error(&error, path, options.version_id.as_deref())
        })?;
        if response.delete_marker().unwrap_or(false) {
            return Err(Error::DeleteMarker {
                path: path.to_string(),
                version_id: response
                    .version_id()
                    .or(options.version_id.as_deref())
                    .unwrap_or("unknown")
                    .to_string(),
            });
        }
        let mut body = response.body;
        let mut bytes_written = 0u64;

        while let Some(chunk) = body
            .try_next()
            .await
            .map_err(|error| Error::Network(error.to_string()))?
        {
            let remaining = max_bytes
                .map(|limit| limit.saturating_sub(bytes_written))
                .unwrap_or(chunk.len() as u64);
            let write_len = chunk.len().min(remaining as usize);
            writer.write_all(&chunk[..write_len]).await?;
            bytes_written += write_len as u64;
            if max_bytes.is_some_and(|limit| bytes_written >= limit) {
                break;
            }
        }
        writer.flush().await?;

        Ok(bytes_written)
    }

    pub async fn put_object_if_absent(
        &self,
        path: &RemotePath,
        data: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<ObjectInfo> {
        let size = data.len() as i64;
        let mut request = self
            .inner
            .put_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .if_none_match("*")
            .body(aws_sdk_s3::primitives::ByteStream::from(data));
        if let Some(content_type) = content_type {
            request = request.content_type(content_type);
        }

        let response = request.send().await.map_err(|error| {
            if let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &error
                && service_error.raw().status().as_u16() == 412
            {
                Error::Conflict(format!("Object already exists: {path}"))
            } else {
                Error::Network(Self::format_sdk_error(&error))
            }
        })?;
        let mut info = ObjectInfo::file(&path.key, size);
        info.etag = response
            .e_tag()
            .map(|etag| etag.trim_matches('"').to_string());
        info.version_id = response.version_id().map(ToString::to_string);
        info.last_modified = Some(jiff::Timestamp::now());
        Ok(info)
    }

    pub async fn delete_object_if_match(&self, path: &RemotePath, etag: &str) -> Result<()> {
        self.inner
            .delete_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .if_match(etag)
            .send()
            .await
            .map_err(|error| {
                if let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &error
                    && service_error.raw().status().as_u16() == 412
                {
                    Error::Conflict(format!("Object changed before deletion: {path}"))
                } else {
                    Error::Network(Self::format_sdk_error(&error))
                }
            })?;
        Ok(())
    }

    /// Delete an object with version, governance, and RustFS force-delete options.
    ///
    /// This compatibility wrapper preserves the original unit result. Call
    /// [`Self::delete_object_with_result`] when version-aware response fields are needed.
    pub async fn delete_object_with_options(
        &self,
        path: &RemotePath,
        options: DeleteRequestOptions,
    ) -> Result<()> {
        self.delete_object_with_result(path, options).await?;
        Ok(())
    }

    /// Delete an object and preserve the returned version and delete-marker fields.
    pub async fn delete_object_with_result(
        &self,
        path: &RemotePath,
        options: DeleteRequestOptions,
    ) -> Result<DeletedObject> {
        let mut builder = self
            .inner
            .delete_object()
            .bucket(&path.bucket)
            .key(&path.key);
        if let Some(version_id) = &options.version_id {
            builder = builder.version_id(version_id);
        }
        if options.bypass_governance {
            builder = builder.bypass_governance_retention(true);
        }
        let mut request = builder.customize();

        if options.force_delete {
            request = request.mutate_request(|request| {
                request
                    .headers_mut()
                    .insert(RUSTFS_FORCE_DELETE_HEADER, "true");
            });
        }
        let response = request.send().await.map_err(|error| {
            Self::map_object_request_error(&error, path, options.version_id.as_deref())
        })?;

        Ok(DeletedObject {
            key: path.key.clone(),
            version_id: response
                .version_id()
                .or(options.version_id.as_deref())
                .map(ToString::to_string),
            is_delete_marker: response.delete_marker().unwrap_or(false),
        })
    }

    /// Delete multiple objects with governance and RustFS force-delete options.
    pub async fn delete_objects_with_options(
        &self,
        bucket: &str,
        keys: Vec<String>,
        options: DeleteRequestOptions,
    ) -> Result<Vec<String>> {
        if keys.is_empty() {
            return Ok(vec![]);
        }
        if options.version_id.is_some() {
            return Err(Error::InvalidPath(
                "Batch key deletion cannot apply one version ID to multiple objects".to_string(),
            ));
        }

        let identifiers = keys
            .into_iter()
            .map(|key| ObjectVersionIdentifier {
                key,
                version_id: None,
                is_delete_marker: false,
            })
            .collect();
        let result = self
            .delete_object_versions_with_options(bucket, identifiers, options)
            .await?;

        if !result.failures.is_empty() {
            let error_keys: Vec<&str> = result
                .failures
                .iter()
                .map(|failure| failure.key.as_str())
                .collect();
            tracing::warn!("Failed to delete some objects: {:?}", error_keys);
        }

        Ok(result
            .deleted
            .into_iter()
            .map(|deleted| deleted.key)
            .collect())
    }

    /// Delete exact object versions and delete markers with optional governance bypass.
    pub async fn delete_object_versions_with_options(
        &self,
        bucket: &str,
        objects: Vec<ObjectVersionIdentifier>,
        options: DeleteRequestOptions,
    ) -> Result<DeleteObjectsResult> {
        use aws_sdk_s3::types::{Delete, ObjectIdentifier};

        if objects.is_empty() {
            return Ok(DeleteObjectsResult::default());
        }
        if options.version_id.is_some() {
            return Err(Error::InvalidPath(
                "Multi-object version deletion requires version IDs on each object identifier"
                    .to_string(),
            ));
        }

        let sdk_objects = objects
            .iter()
            .map(|object| {
                let mut builder = ObjectIdentifier::builder().key(&object.key);
                if let Some(version_id) = &object.version_id {
                    builder = builder.version_id(version_id);
                }
                builder.build().map_err(|error| {
                    Error::General(format!("invalid delete object identifier: {error}"))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let delete = Delete::builder()
            .set_objects(Some(sdk_objects))
            .build()
            .map_err(|error| Error::General(error.to_string()))?;

        let mut builder = self.inner.delete_objects().bucket(bucket).delete(delete);
        if options.bypass_governance {
            builder = builder.bypass_governance_retention(true);
        }
        let mut request = builder.customize();
        if options.force_delete {
            request = request.mutate_request(|request| {
                request
                    .headers_mut()
                    .insert(RUSTFS_FORCE_DELETE_HEADER, "true");
            });
        }
        let first = &objects[0];
        let error_path = RemotePath::new(&self.alias.name, bucket, &first.key);
        let response = request.send().await.map_err(|error| {
            Self::map_object_request_error(&error, &error_path, first.version_id.as_deref())
        })?;

        let deleted = response
            .deleted()
            .iter()
            .filter_map(|entry| {
                let key = entry.key()?.to_string();
                let version_id = entry
                    .version_id()
                    .or(entry.delete_marker_version_id())
                    .map(ToString::to_string);
                let requested_marker = objects.iter().any(|object| {
                    object.key == key && object.version_id == version_id && object.is_delete_marker
                });
                Some(DeletedObject {
                    key,
                    version_id,
                    is_delete_marker: entry.delete_marker().unwrap_or(false) || requested_marker,
                })
            })
            .collect();
        let failures = response
            .errors()
            .iter()
            .map(|entry| DeleteObjectFailure {
                key: entry.key().unwrap_or_default().to_string(),
                version_id: entry.version_id().map(ToString::to_string),
                code: entry.code().map(ToString::to_string),
                message: entry.message().map(ToString::to_string),
            })
            .collect();

        Ok(DeleteObjectsResult { deleted, failures })
    }

    /// Format AWS SDK error into a detailed error message
    fn format_sdk_error<E>(error: &aws_sdk_s3::error::SdkError<E>) -> String
    where
        E: std::fmt::Display + ProvideErrorMetadata,
    {
        match error {
            aws_sdk_s3::error::SdkError::ServiceError(service_err) => {
                let err = service_err.err();
                let meta = service_err.raw();
                let header_code = meta
                    .headers()
                    .get("x-amz-error-code")
                    .and_then(|value| std::str::from_utf8(value.as_bytes()).ok());
                let code = err.code().or(header_code);
                let mut details = vec![format!("status: {}", meta.status().as_u16())];
                if let Some(code) = code {
                    details.push(format!("code: {code}"));
                }
                format!("Service error: {err} ({})", details.join(", "))
            }
            aws_sdk_s3::error::SdkError::ConstructionFailure(err) => {
                format!("Request construction failed: {:?}", err)
            }
            aws_sdk_s3::error::SdkError::TimeoutError(_) => "Request timeout".to_string(),
            aws_sdk_s3::error::SdkError::DispatchFailure(err) => {
                format!("Network dispatch error: {:?}", err)
            }
            aws_sdk_s3::error::SdkError::ResponseError(err) => {
                format!("Response error: {:?}", err)
            }
            _ => error.to_string(),
        }
    }

    fn map_object_request_error<E>(
        error: &aws_sdk_s3::error::SdkError<E>,
        path: &RemotePath,
        requested_version: Option<&str>,
    ) -> Error
    where
        E: ProvideErrorMetadata + std::fmt::Display,
    {
        let formatted = Self::format_sdk_error(error);

        if let aws_sdk_s3::error::SdkError::ServiceError(service_error) = error {
            let raw = service_error.raw();
            let code = service_error.err().code().or_else(|| {
                raw.headers()
                    .get("x-amz-error-code")
                    .and_then(|value| std::str::from_utf8(value.as_bytes()).ok())
            });
            let status = raw.status().as_u16();
            if matches!(code, Some("InvalidStorageClass")) {
                return Error::UnsupportedFeature(
                    "The S3 endpoint rejected the requested storage class".to_string(),
                );
            }
            if matches!(code, Some("BadDigest") | Some("InvalidDigest")) {
                return Error::Conflict(
                    "The S3 endpoint rejected the supplied object checksum".to_string(),
                );
            }
            if matches!(status, 409 | 412)
                || matches!(
                    code,
                    Some("ConditionalRequestConflict") | Some("PreconditionFailed")
                )
            {
                return Error::Conflict(formatted);
            }
            if status == 401 || matches!(code, Some("Unauthorized")) {
                return Error::Auth(formatted);
            }

            let version_header = raw
                .headers()
                .get("x-amz-version-id")
                .and_then(|value| std::str::from_utf8(value.as_bytes()).ok());
            let is_delete_marker = raw
                .headers()
                .get("x-amz-delete-marker")
                .and_then(|value| std::str::from_utf8(value.as_bytes()).ok())
                .is_some_and(|value| value.eq_ignore_ascii_case("true"));

            let service_message = service_error.err().message().unwrap_or(formatted.as_str());
            if Self::is_retention_denial(service_message) {
                return Error::GovernanceDenied {
                    path: path.to_string(),
                    version_id: requested_version.map(ToString::to_string),
                };
            }

            if matches!(code, Some("AccessDenied") | Some("Forbidden")) || status == 403 {
                return Error::Auth(formatted);
            }

            if is_delete_marker {
                return Error::DeleteMarker {
                    path: path.to_string(),
                    version_id: version_header
                        .or(requested_version)
                        .unwrap_or("unknown")
                        .to_string(),
                };
            }

            if matches!(code, Some("NoSuchVersion"))
                || (requested_version.is_some()
                    && (matches!(code, Some("NoSuchKey") | Some("NotFound")) || status == 404))
            {
                return Error::VersionNotFound {
                    path: path.to_string(),
                    version_id: requested_version.unwrap_or("unknown").to_string(),
                };
            }

            if matches!(code, Some("NoSuchKey") | Some("NotFound")) || status == 404 {
                return Error::NotFound(path.to_string());
            }
        }

        if Self::is_retention_denial(&formatted) {
            return Error::GovernanceDenied {
                path: path.to_string(),
                version_id: requested_version.map(ToString::to_string),
            };
        }
        if formatted.contains("AccessDenied")
            || formatted.contains("Forbidden")
            || formatted.contains("Unauthorized")
        {
            return Error::Auth(formatted);
        }
        if formatted.contains("NoSuchVersion")
            || (requested_version.is_some()
                && (formatted.contains("NoSuchKey") || formatted.contains("NotFound")))
        {
            return Error::VersionNotFound {
                path: path.to_string(),
                version_id: requested_version.unwrap_or("unknown").to_string(),
            };
        }
        if formatted.contains("NoSuchKey") || formatted.contains("NotFound") {
            return Error::NotFound(path.to_string());
        }
        Error::Network(formatted)
    }

    fn map_object_lock_write_error<E>(
        error: &aws_sdk_s3::error::SdkError<E>,
        destination: &RemotePath,
        fallback_path: &RemotePath,
        fallback_version: Option<&str>,
    ) -> Error
    where
        E: ProvideErrorMetadata + std::fmt::Display,
    {
        if let aws_sdk_s3::error::SdkError::ServiceError(service_error) = error {
            let raw = service_error.raw();
            let code = service_error.err().code().or_else(|| {
                raw.headers()
                    .get("x-amz-error-code")
                    .and_then(|value| std::str::from_utf8(value.as_bytes()).ok())
            });
            let status = raw.status().as_u16();
            let formatted = Self::format_sdk_error(error);
            let message = service_error
                .err()
                .message()
                .unwrap_or(formatted.as_str())
                .to_ascii_lowercase();

            if status == 501 || matches!(code, Some("NotImplemented")) {
                return Error::UnsupportedFeature(
                    "The S3 endpoint does not support atomic Object Lock writes".to_string(),
                );
            }
            if matches!(code, Some("ObjectLockConfigurationNotFoundError"))
                || (matches!(code, Some("InvalidRequest") | Some("InvalidBucketState"))
                    && message.contains("object lock")
                    && (message.contains("not enabled")
                        || message.contains("not configured")
                        || message.contains("configuration")))
            {
                return Error::UnsupportedFeature(
                    "Object Lock is not enabled for the destination bucket".to_string(),
                );
            }
            if status == 401
                || status == 403
                || matches!(
                    code,
                    Some("Unauthorized") | Some("AccessDenied") | Some("Forbidden")
                )
            {
                return Error::Auth(formatted);
            }
            if message.contains("compliance") {
                return Error::Conflict(format!(
                    "Compliance retention rejected object creation: {destination}"
                ));
            }
            if message.contains("governance")
                || message.contains("retention")
                || message.contains("object lock")
                || message.contains("worm")
            {
                return Error::Conflict(format!(
                    "Object Lock policy rejected object creation: {destination}"
                ));
            }
        }
        Self::map_object_request_error(error, fallback_path, fallback_version)
    }

    fn map_transfer_write_error<E>(
        error: &aws_sdk_s3::error::SdkError<E>,
        destination: &RemotePath,
        options: &ObjectWriteOptions,
    ) -> Error
    where
        E: ProvideErrorMetadata + std::fmt::Display,
    {
        if options.retention.is_some() || options.legal_hold.is_some() {
            Self::map_object_lock_write_error(error, destination, destination, None)
        } else {
            Self::map_object_request_error(error, destination, None)
        }
    }

    fn map_bucket_object_lock_error<E>(
        error: &aws_sdk_s3::error::SdkError<E>,
        bucket: &str,
    ) -> Error
    where
        E: ProvideErrorMetadata + std::fmt::Display,
    {
        let formatted = Self::format_sdk_error(error);
        if let aws_sdk_s3::error::SdkError::ServiceError(service_error) = error {
            let raw = service_error.raw();
            let code = service_error.err().code().or_else(|| {
                raw.headers()
                    .get("x-amz-error-code")
                    .and_then(|value| std::str::from_utf8(value.as_bytes()).ok())
            });
            let status = raw.status().as_u16();
            if status == 501 || matches!(code, Some("NotImplemented")) {
                return Error::UnsupportedFeature(
                    "The S3 endpoint does not support bucket Object Lock configuration".to_string(),
                );
            }
            if status == 401
                || status == 403
                || matches!(
                    code,
                    Some("Unauthorized") | Some("AccessDenied") | Some("Forbidden")
                )
            {
                return Error::Auth(formatted);
            }
            if status == 404 || matches!(code, Some("NoSuchBucket")) {
                return Error::NotFound(format!("Bucket not found: {bucket}"));
            }
            if matches!(code, Some("InvalidRequest") | Some("InvalidBucketState")) {
                return Error::Conflict(formatted);
            }
        }

        if formatted.contains("NotImplemented") {
            Error::UnsupportedFeature(
                "The S3 endpoint does not support bucket Object Lock configuration".to_string(),
            )
        } else {
            Error::Network(formatted)
        }
    }

    fn redact_sensitive_error(&self, error: Error) -> Error {
        match error {
            Error::Config(message) => Error::Config(self.redact_sensitive_text(message)),
            Error::InvalidPath(message) => Error::InvalidPath(self.redact_sensitive_text(message)),
            Error::AliasNotFound(message) => {
                Error::AliasNotFound(self.redact_sensitive_text(message))
            }
            Error::AliasExists(message) => Error::AliasExists(self.redact_sensitive_text(message)),
            Error::Auth(message) => Error::Auth(self.redact_sensitive_text(message)),
            Error::VersionNotFound { path, version_id } => Error::VersionNotFound {
                path: self.redact_sensitive_text(path),
                version_id: self.redact_sensitive_text(version_id),
            },
            Error::DeleteMarker { path, version_id } => Error::DeleteMarker {
                path: self.redact_sensitive_text(path),
                version_id: self.redact_sensitive_text(version_id),
            },
            Error::GovernanceDenied { path, version_id } => Error::GovernanceDenied {
                path: self.redact_sensitive_text(path),
                version_id: version_id.map(|value| self.redact_sensitive_text(value)),
            },
            Error::Network(message) => Error::Network(self.redact_sensitive_text(message)),
            Error::Conflict(message) => Error::Conflict(self.redact_sensitive_text(message)),
            Error::UnsupportedFeature(message) => {
                Error::UnsupportedFeature(self.redact_sensitive_text(message))
            }
            Error::General(message) => Error::General(self.redact_sensitive_text(message)),
            Error::NotFound(message) => Error::NotFound(self.redact_sensitive_text(message)),
            Error::RequestRejected(message) => {
                Error::RequestRejected(self.redact_sensitive_text(message))
            }
            Error::Interrupted(message) => Error::Interrupted(self.redact_sensitive_text(message)),
            other => other,
        }
    }

    fn redact_sse_customer_error(
        &self,
        error: Error,
        customer_key: Option<&SseCustomerKey>,
    ) -> Error {
        let error = self.redact_sensitive_error(error);
        let Some(customer_key) = customer_key else {
            return error;
        };
        let headers = SseCustomerHeaders::new(customer_key);
        let redact = |mut message: String| {
            for value in headers.redaction_values() {
                if !value.is_empty() {
                    message = message.replace(value, "[REDACTED]");
                }
            }
            message
        };
        match error {
            Error::Config(message) => Error::Config(redact(message)),
            Error::InvalidPath(message) => Error::InvalidPath(redact(message)),
            Error::AliasNotFound(message) => Error::AliasNotFound(redact(message)),
            Error::AliasExists(message) => Error::AliasExists(redact(message)),
            Error::Auth(message) => Error::Auth(redact(message)),
            Error::VersionNotFound { path, version_id } => Error::VersionNotFound {
                path: redact(path),
                version_id: redact(version_id),
            },
            Error::DeleteMarker { path, version_id } => Error::DeleteMarker {
                path: redact(path),
                version_id: redact(version_id),
            },
            Error::GovernanceDenied { path, version_id } => Error::GovernanceDenied {
                path: redact(path),
                version_id: version_id.map(redact),
            },
            Error::Network(message) => Error::Network(redact(message)),
            Error::Conflict(message) => Error::Conflict(redact(message)),
            Error::UnsupportedFeature(message) => Error::UnsupportedFeature(redact(message)),
            Error::General(message) => Error::General(redact(message)),
            Error::NotFound(message) => Error::NotFound(redact(message)),
            Error::RequestRejected(message) => Error::RequestRejected(redact(message)),
            Error::Interrupted(message) => Error::Interrupted(redact(message)),
            other => other,
        }
    }

    fn redact_object_lock_service_error<E>(
        &self,
        sdk_error: &aws_sdk_s3::error::SdkError<E>,
        error: Error,
    ) -> Error
    where
        E: ProvideErrorMetadata + std::fmt::Display,
    {
        let (code, status, detail) = match sdk_error {
            aws_sdk_s3::error::SdkError::ServiceError(service_error) => {
                let raw = service_error.raw();
                let code = service_error.err().code().or_else(|| {
                    raw.headers()
                        .get("x-amz-error-code")
                        .and_then(|value| std::str::from_utf8(value.as_bytes()).ok())
                });
                (
                    code.map(str::to_string),
                    Some(raw.status().as_u16()),
                    service_error.err().message().map(str::to_string),
                )
            }
            _ => (None, None, None),
        };
        let error = if status == Some(501) || code.as_deref() == Some("NotImplemented") {
            match error {
                existing @ Error::UnsupportedFeature(_) => existing,
                _ => Error::UnsupportedFeature(
                    "The S3 endpoint does not support this Object Lock operation".to_string(),
                ),
            }
        } else if matches!(
            code.as_deref(),
            Some("InvalidRequest") | Some("InvalidBucketState")
        ) {
            match error {
                Error::Network(message) => Error::Conflict(message),
                Error::GovernanceDenied { .. } => {
                    Error::Conflict("The server rejected the Object Lock request".to_string())
                }
                other => other,
            }
        } else {
            error
        };
        let error = match detail.as_deref().filter(|detail| !detail.is_empty()) {
            Some(detail) => match error {
                Error::Auth(message) => Error::Auth(Self::append_service_detail(message, detail)),
                Error::Network(message) => {
                    Error::Network(Self::append_service_detail(message, detail))
                }
                Error::Conflict(message) => {
                    Error::Conflict(Self::append_service_detail(message, detail))
                }
                Error::General(message) => {
                    Error::General(Self::append_service_detail(message, detail))
                }
                other => other,
            },
            None => error,
        };
        self.redact_sensitive_error(error)
    }

    fn append_service_detail(message: String, detail: &str) -> String {
        if message.contains(detail) {
            message
        } else {
            format!("{message}: {detail}")
        }
    }

    fn redact_sensitive_text(&self, mut message: String) -> String {
        for header in &self.request_headers {
            if !header.value.is_empty() {
                message = message.replace(&header.value, "[REDACTED]");
            }
        }
        for value in [&self.alias.access_key, &self.alias.secret_key] {
            if !value.is_empty() {
                message = message.replace(value, "[REDACTED]");
            }
        }
        message
    }

    fn is_retention_denial(message: &str) -> bool {
        let normalized = message.to_ascii_lowercase();
        normalized.contains("governance")
            || normalized.contains("retention")
            || normalized.contains("object lock")
            || normalized.contains("worm")
    }

    fn should_use_multipart(file_size: u64) -> bool {
        file_size > SINGLE_PUT_OBJECT_MAX_SIZE
    }

    fn sha256_hash(body: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(body);
        hex::encode(hasher.finalize())
    }

    fn request_host(&self, url: &reqwest::Url) -> Result<String> {
        let host = url
            .host_str()
            .ok_or_else(|| Error::Network("Missing host in request URL".to_string()))?;
        Ok(match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        })
    }

    fn replication_url(&self, bucket: &str) -> Result<reqwest::Url> {
        let mut url =
            reqwest::Url::parse(self.alias.endpoint.trim_end_matches('/')).map_err(|e| {
                Error::Network(format!("Invalid endpoint '{}': {e}", self.alias.endpoint))
            })?;

        {
            let mut segments = url.path_segments_mut().map_err(|_| {
                Error::Network(format!(
                    "Endpoint '{}' does not support path-style bucket operations",
                    self.alias.endpoint
                ))
            })?;
            segments.pop_if_empty();
            segments.push(bucket);
        }

        url.set_query(Some("replication="));
        Ok(url)
    }

    fn replication_extension_url(
        &self,
        bucket: &str,
        marker: &str,
        query: &[(&str, String)],
    ) -> Result<reqwest::Url> {
        let mut url =
            reqwest::Url::parse(self.alias.endpoint.trim_end_matches('/')).map_err(|error| {
                Error::Network(format!(
                    "Invalid endpoint '{}': {error}",
                    self.alias.endpoint
                ))
            })?;
        {
            let mut segments = url.path_segments_mut().map_err(|_| {
                Error::Network(format!(
                    "Endpoint '{}' does not support path-style bucket operations",
                    self.alias.endpoint
                ))
            })?;
            segments.pop_if_empty();
            segments.push(bucket);
        }

        url.set_query(Some(marker));
        if !query.is_empty() {
            let mut serializer = url.query_pairs_mut();
            for (name, value) in query {
                serializer.append_pair(name, value);
            }
        }
        Ok(url)
    }

    async fn signed_replication_extension_request(
        &self,
        method: Method,
        url: reqwest::Url,
    ) -> Result<Vec<u8>> {
        let body = [];
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amz-content-sha256",
            HeaderValue::from_str(&Self::sha256_hash(&body))
                .map_err(|error| Error::Auth(format!("Invalid content hash header: {error}")))?,
        );
        headers.insert(
            "host",
            HeaderValue::from_str(&self.request_host(&url)?)
                .map_err(|error| Error::Auth(format!("Invalid host header: {error}")))?,
        );
        for header in &self.request_headers {
            let name = HeaderName::from_bytes(header.name.as_bytes())
                .map_err(|error| Error::Auth(format!("Invalid custom header name: {error}")))?;
            let value = HeaderValue::from_str(&header.value)
                .map_err(|error| Error::Auth(format!("Invalid custom header value: {error}")))?;
            headers.insert(name, value);
        }

        let signed_headers = self
            .sign_xml_request(&method, url.as_str(), &headers, &body)
            .await?;
        let mut request = self.xml_http_client.request(method, url);
        for (name, value) in &signed_headers {
            request = request.header(name, value);
        }

        let response = request
            .send()
            .await
            .map_err(|error| Error::Network(format!("Replication request failed: {error}")))?;
        let status = response.status();
        if response
            .content_length()
            .is_some_and(|length| length > REPLICATION_EXTENSION_BODY_LIMIT)
        {
            return Err(Error::General(format!(
                "Replication response exceeds the {} byte limit",
                REPLICATION_EXTENSION_BODY_LIMIT
            )));
        }

        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.try_next().await.map_err(|error| {
            Error::Network(format!("Failed to read replication response: {error}"))
        })? {
            let next_len = bytes.len().saturating_add(chunk.len());
            if next_len as u64 > REPLICATION_EXTENSION_BODY_LIMIT {
                return Err(Error::General(format!(
                    "Replication response exceeds the {} byte limit",
                    REPLICATION_EXTENSION_BODY_LIMIT
                )));
            }
            bytes.extend_from_slice(&chunk);
        }

        if !status.is_success() {
            return Err(self.map_replication_extension_error(status, &bytes));
        }
        Ok(bytes)
    }

    fn map_replication_extension_error(&self, status: reqwest::StatusCode, body: &[u8]) -> Error {
        let text = String::from_utf8_lossy(body);
        let parsed = from_xml_str::<S3ExtensionErrorDto>(&text).ok();
        let code = parsed.as_ref().map(|error| error.code.as_str());
        let detail = parsed
            .as_ref()
            .map(|error| error.message.trim())
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| text.trim());
        let mut message = if detail.is_empty() {
            format!("HTTP {}", status.as_u16())
        } else if let Some(code) = code {
            format!("{code}: {detail}")
        } else {
            format!("HTTP {}: {detail}", status.as_u16())
        };
        for sensitive in [&self.alias.access_key, &self.alias.secret_key] {
            if !sensitive.is_empty() {
                message = message.replace(sensitive, "[REDACTED]");
            }
        }

        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
            || matches!(code, Some("AccessDenied" | "Unauthorized" | "Forbidden"))
        {
            Error::Auth(message)
        } else if status == reqwest::StatusCode::NOT_IMPLEMENTED
            || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
            || matches!(code, Some("NotImplemented" | "MethodNotAllowed"))
        {
            Error::UnsupportedFeature(message)
        } else if matches!(
            code,
            Some(
                "NoSuchBucket"
                    | "ReplicationConfigurationNotFoundError"
                    | "ReplicationConfigurationNotFound"
            )
        ) {
            Error::NotFound(message)
        } else if status == reqwest::StatusCode::NOT_FOUND {
            Error::UnsupportedFeature(message)
        } else if status == reqwest::StatusCode::CONFLICT
            || status == reqwest::StatusCode::BAD_REQUEST
            || matches!(code, Some("InvalidRequest" | "InvalidBucketState"))
        {
            Error::Conflict(message)
        } else if status.is_server_error() {
            Error::Network(message)
        } else {
            Error::General(message)
        }
    }

    fn parse_replication_check_response(&self, body: &[u8]) -> Result<ReplicationCheckResult> {
        if body.is_empty() {
            return Ok(ReplicationCheckResult::legacy_success());
        }

        let mut result = serde_json::from_slice::<ReplicationCheckResult>(body).map_err(|_| {
            Error::General("Malformed structured replication check response".to_string())
        })?;
        if result.contract_version.is_some_and(|version| version != 1)
            || !result.active_mutation
            || result.probe_namespace != REPLICATION_CHECK_PROBE_NAMESPACE
            || result.mutation_description.is_empty()
            || result.mutation_description.len() > REPLICATION_CHECK_DESCRIPTION_LIMIT
            || contains_control_characters(&result.mutation_description)
            || result.targets.is_empty()
        {
            return Err(Error::General(
                "Malformed structured replication check response".to_string(),
            ));
        }

        let expected_status = if result
            .targets
            .iter()
            .all(|target| target.status == ReplicationCheckStatus::Ok)
        {
            ReplicationCheckStatus::Ok
        } else {
            ReplicationCheckStatus::Failed
        };
        if result.status != expected_status {
            return Err(Error::General(
                "Malformed structured replication check response".to_string(),
            ));
        }

        for target in &mut result.targets {
            if target.target_arn.is_empty()
                || target.bucket.is_empty()
                || contains_control_characters(&target.target_arn)
                || contains_control_characters(&target.bucket)
            {
                return Err(Error::General(
                    "Malformed structured replication check response".to_string(),
                ));
            }
            target.target_arn = self.redact_sensitive_text(std::mem::take(&mut target.target_arn));
            target.bucket = self.redact_sensitive_text(std::mem::take(&mut target.bucket));
            validate_replication_check_error(target.error.as_deref())?;
            if let Some(error) = target.error.take() {
                target.error = Some(self.sanitize_replication_check_error(error));
            }
            for phase in replication_check_phases_mut(&mut target.phases) {
                validate_replication_check_phase(phase)?;
                if let Some(error) = phase.error.take() {
                    phase.error = Some(self.sanitize_replication_check_error(error));
                }
            }

            let any_failed = replication_check_phases(&target.phases)
                .into_iter()
                .any(|phase| phase.status == ReplicationCheckPhaseState::Failed);
            if (target.status == ReplicationCheckStatus::Failed)
                != (any_failed || target.error.is_some())
            {
                return Err(Error::General(
                    "Malformed structured replication check response".to_string(),
                ));
            }
        }

        result.mutation_description = self.redact_sensitive_text(result.mutation_description);
        Ok(result)
    }

    fn sanitize_replication_check_error(&self, error: String) -> String {
        let redacted = self.redact_sensitive_text(error);
        let lower = redacted.to_ascii_lowercase();
        if [
            "http://",
            "https://",
            "authorization",
            "x-amz-credential",
            "x-amz-signature",
            "secret-key",
            "access-key",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        {
            "server reported a redacted replication check failure".to_string()
        } else {
            redacted
        }
    }

    fn parse_replication_timestamp(
        value: Option<String>,
        field: &str,
    ) -> Result<Option<jiff::Timestamp>> {
        value
            .map(|value| {
                jiff::Timestamp::from_str(&value).map_err(|error| {
                    Error::General(format!("Malformed replication {field}: {error}"))
                })
            })
            .transpose()
    }

    fn convert_resync_status_target(
        target: ReplicationResyncTargetDto,
    ) -> Result<ReplicationResyncTargetStatus> {
        if target.arn.is_empty() {
            return Err(Error::General(
                "Malformed replication status target: missing ARN".to_string(),
            ));
        }
        let server_state = target.status.unwrap_or_default();
        let state = ReplicationResyncState::from_server(&server_state);
        let nonnegative = |value: Option<i64>, field: &str| {
            u64::try_from(value.unwrap_or_default()).map_err(|_| {
                Error::General(format!("Malformed replication status: negative {field}"))
            })
        };

        Ok(ReplicationResyncTargetStatus {
            target_arn: target.arn,
            reset_id: target.reset_id,
            reset_before: Self::parse_replication_timestamp(
                target.reset_before_date,
                "reset-before timestamp",
            )?,
            started_at: Self::parse_replication_timestamp(target.start_time, "start timestamp")?,
            last_updated_at: Self::parse_replication_timestamp(
                target.end_time,
                "last-update timestamp",
            )?,
            state,
            server_state,
            replicated_count: nonnegative(target.replicated_count, "replicated count")?,
            replicated_size: nonnegative(target.replicated_size, "replicated size")?,
            failed_count: nonnegative(target.failed_count, "failed count")?,
            failed_size: nonnegative(target.failed_size, "failed size")?,
            current_bucket: target.bucket.filter(|value| !value.is_empty()),
            current_object: target.object.filter(|value| !value.is_empty()),
            error: target.error.filter(|value| !value.is_empty()),
        })
    }

    fn cors_url(&self, bucket: &str) -> Result<reqwest::Url> {
        let mut url =
            reqwest::Url::parse(self.alias.endpoint.trim_end_matches('/')).map_err(|e| {
                Error::Network(format!("Invalid endpoint '{}': {e}", self.alias.endpoint))
            })?;

        {
            let mut segments = url.path_segments_mut().map_err(|_| {
                Error::Network(format!(
                    "Endpoint '{}' does not support path-style bucket operations",
                    self.alias.endpoint
                ))
            })?;
            segments.pop_if_empty();
            segments.push(bucket);
        }

        url.set_query(Some("cors="));
        Ok(url)
    }

    async fn sign_xml_request(
        &self,
        method: &Method,
        url: &str,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<HeaderMap> {
        if self.alias.anonymous {
            return Ok(headers.clone());
        }

        let credentials = Credentials::new(
            &self.alias.access_key,
            &self.alias.secret_key,
            None,
            None,
            "s3-xml-client",
        );

        let identity = credentials.into();
        let mut signing_settings = SigningSettings::default();
        signing_settings.signature_location = SignatureLocation::Headers;

        let signing_params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.alias.region)
            .name(S3_SERVICE_NAME)
            .time(std::time::SystemTime::now())
            .settings(signing_settings)
            .build()
            .map_err(|e| Error::Auth(format!("Failed to build signing params: {e}")))?;

        let header_pairs: Vec<(&str, &str)> = headers
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str(), v)))
            .collect();

        let signable_request = SignableRequest::new(
            method.as_str(),
            url,
            header_pairs.into_iter(),
            SignableBody::Bytes(body),
        )
        .map_err(|e| Error::Auth(format!("Failed to create signable request: {e}")))?;

        let (signing_instructions, _) = sign(signable_request, &signing_params.into())
            .map_err(|e| Error::Auth(format!("Failed to sign request: {e}")))?
            .into_parts();

        let mut signed_headers = headers.clone();
        for (name, value) in signing_instructions.headers() {
            let header_name = HeaderName::try_from(name.to_string())
                .map_err(|e| Error::Auth(format!("Invalid header name: {e}")))?;
            let header_value = HeaderValue::try_from(value.to_string())
                .map_err(|e| Error::Auth(format!("Invalid header value: {e}")))?;
            signed_headers.insert(header_name, header_value);
        }

        Ok(signed_headers)
    }

    async fn xml_request(
        &self,
        method: Method,
        url: reqwest::Url,
        content_type: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> Result<String> {
        let body = body.unwrap_or_default();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amz-content-sha256",
            HeaderValue::from_str(&Self::sha256_hash(&body))
                .map_err(|e| Error::Auth(format!("Invalid content hash header: {e}")))?,
        );
        headers.insert(
            "host",
            HeaderValue::from_str(&self.request_host(&url)?)
                .map_err(|e| Error::Auth(format!("Invalid host header: {e}")))?,
        );

        if let Some(content_type) = content_type {
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_str(content_type)
                    .map_err(|e| Error::Auth(format!("Invalid content type header: {e}")))?,
            );
        }

        for header in &self.request_headers {
            let name = HeaderName::from_bytes(header.name.as_bytes())
                .map_err(|e| Error::Auth(format!("Invalid custom header name: {e}")))?;
            let value = HeaderValue::from_str(&header.value)
                .map_err(|e| Error::Auth(format!("Invalid custom header value: {e}")))?;
            headers.insert(name, value);
        }

        let signed_headers = self
            .sign_xml_request(&method, url.as_str(), &headers, &body)
            .await?;

        let mut request_builder = self.xml_http_client.request(method, url);
        for (name, value) in &signed_headers {
            request_builder = request_builder.header(name, value);
        }
        if !body.is_empty() {
            request_builder = request_builder.body(body);
        }

        let response = request_builder
            .send()
            .await
            .map_err(|e| Error::Network(format!("Request failed: {e}")))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| Error::Network(format!("Failed to read response: {e}")))?;

        if !status.is_success() {
            return Err(Error::Network(format!(
                "HTTP {}: {}",
                status.as_u16(),
                text
            )));
        }

        Ok(text)
    }

    fn bucket_policy_error_kind(
        error_code: Option<&str>,
        status_code: Option<u16>,
        error_text: &str,
    ) -> BucketPolicyErrorKind {
        let error_code = error_code.map(|code| code.to_ascii_lowercase());
        if matches!(
            error_code.as_deref(),
            Some("nosuchbucketpolicy") | Some("nosuchpolicy")
        ) {
            return BucketPolicyErrorKind::MissingPolicy;
        }
        if matches!(error_code.as_deref(), Some("nosuchbucket")) {
            return BucketPolicyErrorKind::MissingBucket;
        }

        let error_text = error_text.to_ascii_lowercase();
        if error_text.contains("nosuchbucketpolicy") || error_text.contains("nosuchpolicy") {
            return BucketPolicyErrorKind::MissingPolicy;
        }
        if error_text.contains("nosuchbucket") {
            return BucketPolicyErrorKind::MissingBucket;
        }
        if status_code == Some(404) {
            return BucketPolicyErrorKind::MissingPolicy;
        }

        BucketPolicyErrorKind::Other
    }

    fn map_get_bucket_policy_error(
        bucket: &str,
        kind: BucketPolicyErrorKind,
        error_text: &str,
    ) -> Result<Option<String>> {
        match kind {
            BucketPolicyErrorKind::MissingPolicy => Ok(None),
            BucketPolicyErrorKind::MissingBucket => {
                Err(Error::NotFound(format!("Bucket not found: {bucket}")))
            }
            BucketPolicyErrorKind::Other => {
                Err(Error::Network(format!("get_bucket_policy: {error_text}")))
            }
        }
    }

    fn map_delete_bucket_policy_error(
        bucket: &str,
        kind: BucketPolicyErrorKind,
        error_text: &str,
    ) -> Result<()> {
        match kind {
            BucketPolicyErrorKind::MissingPolicy => Ok(()),
            BucketPolicyErrorKind::MissingBucket => {
                Err(Error::NotFound(format!("Bucket not found: {bucket}")))
            }
            BucketPolicyErrorKind::Other => Err(Error::General(format!(
                "delete_bucket_policy: {error_text}"
            ))),
        }
    }

    fn extract_notification_filter(
        filter: Option<&aws_sdk_s3::types::NotificationConfigurationFilter>,
    ) -> (Option<String>, Option<String>) {
        let mut prefix = None;
        let mut suffix = None;

        if let Some(key_filter) = filter.and_then(|value| value.key()) {
            for rule in key_filter.filter_rules() {
                match rule.name().map(|name| name.as_str()) {
                    Some("prefix") => {
                        prefix = rule.value().map(ToString::to_string);
                    }
                    Some("suffix") => {
                        suffix = rule.value().map(ToString::to_string);
                    }
                    _ => {}
                }
            }
        }

        (prefix, suffix)
    }

    fn build_notification_filter(
        prefix: Option<&str>,
        suffix: Option<&str>,
    ) -> Option<aws_sdk_s3::types::NotificationConfigurationFilter> {
        use aws_sdk_s3::types::{FilterRule, FilterRuleName, NotificationConfigurationFilter};

        let mut rules = Vec::new();
        if let Some(value) = prefix {
            let rule = FilterRule::builder()
                .name(FilterRuleName::Prefix)
                .value(value)
                .build();
            rules.push(rule);
        }
        if let Some(value) = suffix {
            let rule = FilterRule::builder()
                .name(FilterRuleName::Suffix)
                .value(value)
                .build();
            rules.push(rule);
        }
        if rules.is_empty() {
            return None;
        }

        let key_filter = aws_sdk_s3::types::S3KeyFilter::builder()
            .set_filter_rules(Some(rules))
            .build();
        NotificationConfigurationFilter::builder()
            .key(key_filter)
            .build()
            .into()
    }

    fn event_list_to_strings(events: &[aws_sdk_s3::types::Event]) -> Vec<String> {
        events
            .iter()
            .map(|event| event.as_str().to_string())
            .collect()
    }

    fn strings_to_event_list(events: &[String]) -> Vec<aws_sdk_s3::types::Event> {
        events
            .iter()
            .map(|event| aws_sdk_s3::types::Event::from(event.as_str()))
            .collect()
    }

    fn notifications_equivalent(
        expected: &[BucketNotification],
        actual: &[BucketNotification],
    ) -> bool {
        type CanonicalEntry = (u8, String, Option<String>, Option<String>, Vec<String>);

        fn target_order(target: NotificationTarget) -> u8 {
            match target {
                NotificationTarget::Queue => 0,
                NotificationTarget::Topic => 1,
                NotificationTarget::Lambda => 2,
            }
        }

        fn canonical(notifications: &[BucketNotification]) -> Vec<CanonicalEntry> {
            let mut normalized: Vec<CanonicalEntry> = notifications
                .iter()
                .map(|item| {
                    let mut events = item.events.clone();
                    events.sort();
                    events.dedup();
                    (
                        target_order(item.target),
                        item.arn.clone(),
                        item.prefix.clone(),
                        item.suffix.clone(),
                        events,
                    )
                })
                .collect();
            normalized.sort();
            normalized
        }

        canonical(expected) == canonical(actual)
    }

    async fn read_next_part(
        file: &mut tokio::fs::File,
        file_path: &std::path::Path,
        buffer: &mut [u8],
    ) -> Result<usize> {
        let mut total_read = 0usize;
        while total_read < buffer.len() {
            let bytes_read = file
                .read(&mut buffer[total_read..])
                .await
                .map_err(|e| Error::General(format!("read file '{}': {e}", file_path.display())))?;
            if bytes_read == 0 {
                break;
            }
            total_read += bytes_read;
        }
        Ok(total_read)
    }

    async fn verify_persisted_sha256(
        &self,
        path: &RemotePath,
        version_id: Option<String>,
        expected: &str,
        customer_key: Option<&SseCustomerKey>,
    ) -> Result<()> {
        let persisted = self
            .head_object_transfer_metadata(
                path,
                &TransferReadOptions {
                    version_id,
                    checksum_mode: true,
                    customer_key: customer_key.cloned(),
                },
            )
            .await?
            .checksums
            .into_iter()
            .find(|checksum| checksum.algorithm == ChecksumAlgorithm::Sha256)
            .ok_or_else(|| {
                Error::UnsupportedFeature(
                    "RustFS did not report a persisted SHA-256 checksum; post-write verification is unavailable"
                        .to_string(),
                )
            })?;
        if persisted.value != expected {
            return Err(Error::Conflict(
                "Persisted object SHA-256 checksum does not match the uploaded payload".to_string(),
            ));
        }
        Ok(())
    }

    async fn put_object_single_part_from_path(
        &self,
        path: &RemotePath,
        file_path: &std::path::Path,
        file_size: u64,
        options: PathUploadOptions<'_>,
    ) -> Result<ObjectInfo> {
        let data = tokio::fs::read(file_path)
            .await
            .map_err(|e| Error::General(format!("read file '{}': {e}", file_path.display())))?;
        let requested_checksum = requested_sha256_checksum(&data, options.write.checksum.as_ref())?;
        let body = aws_sdk_s3::primitives::ByteStream::from(data);
        let storage_class = rustfs_storage_class(options.write.storage_class.as_deref())?;

        let mut request = apply_object_attributes_to_put_request(
            apply_object_write_encryption_to_put_request(
                self.inner
                    .put_object()
                    .bucket(&path.bucket)
                    .key(&path.key)
                    .body(body),
                options.write.encryption.as_ref(),
            ),
            options.write.attributes.as_ref(),
        )?
        .set_storage_class(storage_class);

        if let Some(tags) = &options.write.tags {
            request = request.tagging(encode_object_tags(tags));
        }
        if let Some(checksum) = &requested_checksum {
            request = request
                .checksum_algorithm(aws_sdk_s3::types::ChecksumAlgorithm::Sha256)
                .checksum_sha256(checksum);
        }
        request = apply_object_lock_to_put_request(request, options.write)?;
        request = match options.precondition {
            ObjectWritePrecondition::None => request,
            ObjectWritePrecondition::IfAbsent => request.if_none_match("*"),
            ObjectWritePrecondition::IfMatch(etag) => request.if_match(etag),
        };

        let response = request.send().await.map_err(|error| {
            let mapped = if !matches!(options.precondition, ObjectWritePrecondition::None)
                && let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &error
                && matches!(service_error.raw().status().as_u16(), 409 | 412)
            {
                Error::Conflict(format!("Object changed before upload: {path}"))
            } else {
                Self::map_transfer_write_error(&error, path, options.write)
            };
            self.redact_sse_customer_error(
                mapped,
                destination_sse_customer_key(options.write.encryption.as_ref()),
            )
        })?;

        let mut info = ObjectInfo::file(&path.key, file_size as i64);
        if let Some(etag) = response.e_tag() {
            info.etag = Some(etag.trim_matches('"').to_string());
        }
        info.version_id = response.version_id().map(ToString::to_string);
        info.last_modified = Some(jiff::Timestamp::now());
        info.storage_class = options.write.storage_class.clone();

        if let Some(checksum) = requested_checksum {
            self.verify_persisted_sha256(
                path,
                info.version_id.clone(),
                &checksum,
                destination_sse_customer_key(options.write.encryption.as_ref()),
            )
            .await?;
        }
        Ok(info)
    }

    async fn abort_multipart_upload_best_effort(
        &self,
        path: &RemotePath,
        upload_id: &str,
    ) -> MultipartAbortStatus {
        match self
            .inner
            .abort_multipart_upload()
            .bucket(&path.bucket)
            .key(&path.key)
            .upload_id(upload_id)
            .send()
            .await
        {
            Ok(_) => MultipartAbortStatus::Succeeded,
            Err(_) => MultipartAbortStatus::Failed,
        }
    }

    async fn abort_multipart_upload_with_error(
        &self,
        path: &RemotePath,
        upload_id: &str,
        error: Error,
    ) -> Error {
        let abort_status = self
            .abort_multipart_upload_best_effort(path, upload_id)
            .await;
        self.redact_sensitive_error(error.with_multipart_copy_context(upload_id, abort_status))
    }

    async fn put_object_multipart_from_path(
        &self,
        path: &RemotePath,
        file_path: &std::path::Path,
        file_size: u64,
        options: PathUploadOptions<'_>,
        on_progress: impl Fn(u64) + Send,
    ) -> Result<ObjectInfo> {
        use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};

        if options.write.storage_class.is_some() {
            return Err(Error::UnsupportedFeature(
                "RustFS beta.10 does not persist storage class for multipart uploads; tracked by rustfs/backlog#1464"
                    .to_string(),
            ));
        }
        let config = crate::multipart::MultipartConfig::default();
        let part_size = config.calculate_part_size(file_size);
        let part_buffer_size = usize::try_from(part_size)
            .map_err(|_| Error::General(format!("invalid part size: {part_size}")))?;
        let mut file = tokio::fs::File::open(file_path)
            .await
            .map_err(|e| Error::General(format!("open file '{}': {e}", file_path.display())))?;
        let mut chunk = vec![0u8; part_buffer_size];
        let checksum_requested = options.write.checksum.is_some();
        if checksum_requested
            && matches!(
                options.write.checksum,
                Some(ChecksumRequest::Precomputed(_))
            )
        {
            return Err(Error::UnsupportedFeature(
                "Precomputed SHA-256 checksums are not supported for multipart uploads".to_string(),
            ));
        }
        tracing::debug!(file_size, part_size, "Starting multipart upload");

        let mut create_request = apply_object_write_encryption_to_multipart_create_request(
            self.inner
                .create_multipart_upload()
                .bucket(&path.bucket)
                .key(&path.key),
            options.write.encryption.as_ref(),
        );
        if let Some(attributes) = &options.write.attributes {
            create_request =
                apply_object_attributes_to_multipart_create_request(create_request, attributes)?;
        }
        if let Some(tags) = &options.write.tags {
            create_request = create_request.tagging(encode_object_tags(tags));
        }
        if checksum_requested {
            create_request =
                create_request.checksum_algorithm(aws_sdk_s3::types::ChecksumAlgorithm::Sha256);
        }
        create_request =
            apply_object_lock_to_multipart_create_request(create_request, options.write)?;

        let create_response = create_request.send().await.map_err(|error| {
            self.redact_sse_customer_error(
                Self::map_transfer_write_error(&error, path, options.write),
                destination_sse_customer_key(options.write.encryption.as_ref()),
            )
        })?;

        let upload_id = create_response
            .upload_id()
            .ok_or_else(|| Error::General("missing upload id from multipart upload".to_string()))?
            .to_string();

        tracing::debug!(upload_id = %upload_id, "Multipart upload initiated");

        let mut completed_parts = Vec::new();
        let mut part_number: i32 = 1;
        let mut bytes_uploaded: u64 = 0;
        let mut part_digests = Vec::new();

        loop {
            let bytes_read = match Self::read_next_part(&mut file, file_path, &mut chunk).await {
                Ok(bytes_read) => bytes_read,
                Err(error) => {
                    return Err(self
                        .abort_multipart_upload_with_error(path, &upload_id, error)
                        .await);
                }
            };
            // A conditional zero-byte write still needs a multipart completion
            // request, because RustFS evaluates destination preconditions there.
            // S3 permits the final part to be smaller than the minimum part size.
            if bytes_read == 0 && !(file_size == 0 && part_number == 1) {
                break;
            }

            tracing::debug!(part_number, bytes_read, "Uploading part");

            let part_checksum = checksum_requested.then(|| {
                let digest: [u8; 32] = Sha256::digest(&chunk[..bytes_read]).into();
                part_digests.push(digest);
                BASE64_STANDARD.encode(digest)
            });
            let body = aws_sdk_s3::primitives::ByteStream::from(chunk[..bytes_read].to_vec());
            let mut upload_part_request = apply_sse_customer_to_upload_part_request(
                self.inner
                    .upload_part()
                    .bucket(&path.bucket)
                    .key(&path.key)
                    .upload_id(&upload_id)
                    .part_number(part_number)
                    .body(body),
                destination_sse_customer_key(options.write.encryption.as_ref()),
            );
            if let Some(checksum) = &part_checksum {
                upload_part_request = upload_part_request
                    .checksum_algorithm(aws_sdk_s3::types::ChecksumAlgorithm::Sha256)
                    .checksum_sha256(checksum);
            }
            let upload_part_result = upload_part_request.send().await;

            let upload_part_response = match upload_part_result {
                Ok(response) => response,
                Err(e) => {
                    tracing::debug!(
                        upload_id = %upload_id,
                        part_number,
                        "Aborting multipart upload due to error"
                    );
                    let primary = self.redact_sse_customer_error(
                        Self::map_object_request_error(&e, path, None),
                        destination_sse_customer_key(options.write.encryption.as_ref()),
                    );
                    return Err(self
                        .abort_multipart_upload_with_error(path, &upload_id, primary)
                        .await);
                }
            };
            if let Some(expected) = &part_checksum {
                match upload_part_response.checksum_sha256() {
                    Some(actual) if actual == expected => {}
                    Some(_) => {
                        let primary = Error::Conflict(format!(
                            "Persisted multipart part {part_number} SHA-256 checksum does not match the uploaded payload"
                        ));
                        return Err(self
                            .abort_multipart_upload_with_error(path, &upload_id, primary)
                            .await);
                    }
                    None => {
                        let primary = Error::UnsupportedFeature(format!(
                            "RustFS did not report a SHA-256 checksum for multipart part {part_number}"
                        ));
                        return Err(self
                            .abort_multipart_upload_with_error(path, &upload_id, primary)
                            .await);
                    }
                }
            }

            let etag = match upload_part_response.e_tag() {
                Some(value) => value.trim_matches('"').to_string(),
                None => {
                    let primary =
                        Error::General(format!("missing ETag for multipart part {part_number}"));
                    return Err(self
                        .abort_multipart_upload_with_error(path, &upload_id, primary)
                        .await);
                }
            };

            completed_parts.push({
                let mut part = CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(etag);
                if let Some(checksum) = part_checksum {
                    part = part.checksum_sha256(checksum);
                }
                part.build()
            });

            bytes_uploaded += bytes_read as u64;
            on_progress(bytes_uploaded);
            tracing::debug!(part_number, bytes_uploaded, "Part uploaded");

            part_number += 1;
        }

        let completed_upload = CompletedMultipartUpload::builder()
            .set_parts(Some(completed_parts))
            .build();
        let mut complete_request = self
            .inner
            .complete_multipart_upload()
            .bucket(&path.bucket)
            .key(&path.key)
            .upload_id(&upload_id)
            .multipart_upload(completed_upload);
        complete_request = match options.precondition {
            ObjectWritePrecondition::None => complete_request,
            ObjectWritePrecondition::IfAbsent => complete_request.if_none_match("*"),
            ObjectWritePrecondition::IfMatch(etag) => complete_request.if_match(etag),
        };
        let complete_result = complete_request.send().await;

        let complete_response = match complete_result {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(upload_id = %upload_id, "Attempting to abort multipart upload after completion failure");
                let primary = if !matches!(options.precondition, ObjectWritePrecondition::None)
                    && let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &error
                    && matches!(service_error.raw().status().as_u16(), 409 | 412)
                {
                    Error::Conflict(format!("Object changed before upload: {path}"))
                } else {
                    self.redact_sse_customer_error(
                        Self::map_object_request_error(&error, path, None),
                        destination_sse_customer_key(options.write.encryption.as_ref()),
                    )
                };
                return Err(self
                    .abort_multipart_upload_with_error(path, &upload_id, primary)
                    .await);
            }
        };

        tracing::debug!("Multipart upload completed");

        let mut info = ObjectInfo::file(&path.key, file_size as i64);
        if let Some(etag) = complete_response.e_tag() {
            info.etag = Some(etag.trim_matches('"').to_string());
        }
        info.version_id = complete_response.version_id().map(ToString::to_string);
        info.last_modified = Some(jiff::Timestamp::now());

        if checksum_requested {
            let expected_checksum = composite_sha256_checksum(&part_digests);
            self.verify_persisted_sha256(
                path,
                info.version_id.clone(),
                &expected_checksum,
                destination_sse_customer_key(options.write.encryption.as_ref()),
            )
            .await?;
        }
        Ok(info)
    }

    /// Upload a local file path to S3.
    ///
    /// Uses multipart upload for large files to avoid loading the entire file into memory.
    /// Calls `on_progress` after each uploaded part with total bytes sent so far.
    pub async fn put_object_from_path(
        &self,
        path: &RemotePath,
        file_path: &std::path::Path,
        content_type: Option<&str>,
        encryption: Option<&ObjectEncryptionRequest>,
        on_progress: impl Fn(u64) + Send,
    ) -> Result<ObjectInfo> {
        let options = ObjectWriteOptions {
            attributes: Some(ObjectAttributes {
                content_type: content_type.map(ToString::to_string),
                ..ObjectAttributes::default()
            }),
            encryption: encryption.cloned().map(ObjectWriteEncryption::Managed),
            ..ObjectWriteOptions::default()
        };
        self.put_object_from_path_with_condition(
            path,
            file_path,
            &options,
            ObjectWritePrecondition::None,
            on_progress,
        )
        .await
    }

    /// Upload a local file path with advanced transfer-fidelity options.
    ///
    /// Checksum calculation remains streaming for multipart uploads.
    pub async fn put_object_from_path_with_options(
        &self,
        path: &RemotePath,
        file_path: &std::path::Path,
        options: &ObjectWriteOptions,
        on_progress: impl Fn(u64) + Send,
    ) -> Result<ObjectInfo> {
        self.put_object_from_path_with_condition(
            path,
            file_path,
            options,
            ObjectWritePrecondition::None,
            on_progress,
        )
        .await
    }

    /// Upload a local file path only when the destination object does not exist.
    ///
    /// The precondition is applied to `PutObject` for single-part uploads and to
    /// `CompleteMultipartUpload` for multipart uploads, so a concurrent writer
    /// cannot be overwritten between mirror planning and completion.
    pub async fn put_object_from_path_if_absent(
        &self,
        path: &RemotePath,
        file_path: &std::path::Path,
        content_type: Option<&str>,
        encryption: Option<&ObjectEncryptionRequest>,
        on_progress: impl Fn(u64) + Send,
    ) -> Result<ObjectInfo> {
        let options = ObjectWriteOptions {
            attributes: Some(ObjectAttributes {
                content_type: content_type.map(ToString::to_string),
                ..ObjectAttributes::default()
            }),
            encryption: encryption.cloned().map(ObjectWriteEncryption::Managed),
            ..ObjectWriteOptions::default()
        };
        self.put_object_from_path_with_condition(
            path,
            file_path,
            &options,
            ObjectWritePrecondition::IfAbsent,
            on_progress,
        )
        .await
    }

    /// Upload a local file path only when the destination still has `etag`.
    pub async fn put_object_from_path_if_match(
        &self,
        path: &RemotePath,
        file_path: &std::path::Path,
        content_type: Option<&str>,
        encryption: Option<&ObjectEncryptionRequest>,
        etag: &str,
        on_progress: impl Fn(u64) + Send,
    ) -> Result<ObjectInfo> {
        let options = ObjectWriteOptions {
            attributes: Some(ObjectAttributes {
                content_type: content_type.map(ToString::to_string),
                ..ObjectAttributes::default()
            }),
            encryption: encryption.cloned().map(ObjectWriteEncryption::Managed),
            ..ObjectWriteOptions::default()
        };
        self.put_object_from_path_if_match_with_options(
            path,
            file_path,
            &options,
            etag,
            on_progress,
        )
        .await
    }

    /// Upload a local file with explicit write options only when the object is absent.
    pub async fn put_object_from_path_if_absent_with_options(
        &self,
        path: &RemotePath,
        file_path: &std::path::Path,
        options: &ObjectWriteOptions,
        on_progress: impl Fn(u64) + Send,
    ) -> Result<ObjectInfo> {
        self.put_object_from_path_with_condition(
            path,
            file_path,
            options,
            ObjectWritePrecondition::IfAbsent,
            on_progress,
        )
        .await
    }

    /// Upload a local file with explicit write options only when `etag` still matches.
    pub async fn put_object_from_path_if_match_with_options(
        &self,
        path: &RemotePath,
        file_path: &std::path::Path,
        options: &ObjectWriteOptions,
        etag: &str,
        on_progress: impl Fn(u64) + Send,
    ) -> Result<ObjectInfo> {
        self.put_object_from_path_with_condition(
            path,
            file_path,
            options,
            ObjectWritePrecondition::IfMatch(etag),
            on_progress,
        )
        .await
    }

    async fn put_object_from_path_with_condition(
        &self,
        path: &RemotePath,
        file_path: &std::path::Path,
        write: &ObjectWriteOptions,
        precondition: ObjectWritePrecondition<'_>,
        on_progress: impl Fn(u64) + Send,
    ) -> Result<ObjectInfo> {
        validate_attribute_tag_write_options(write)?;
        self.ensure_write_sse_customer_transport(write.encryption.as_ref())?;
        let metadata = tokio::fs::metadata(file_path).await.map_err(|e| {
            Error::General(format!("read metadata for '{}': {e}", file_path.display()))
        })?;
        if !metadata.is_file() {
            return Err(Error::General(format!(
                "source is not a file: {}",
                file_path.display()
            )));
        }

        let file_size = metadata.len();
        let options = PathUploadOptions {
            write,
            precondition,
        };
        // RustFS evaluates write preconditions for multipart completion. Keep
        // ordinary small uploads on PutObject, but route conditional path writes
        // through multipart so mirror retains compare-and-swap semantics on the
        // currently deployed service.
        if Self::should_use_multipart(file_size)
            || !matches!(precondition, ObjectWritePrecondition::None)
        {
            self.put_object_multipart_from_path(path, file_path, file_size, options, on_progress)
                .await
        } else {
            self.put_object_single_part_from_path(path, file_path, file_size, options)
                .await
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_multipart_copy(
        &self,
        src: &RemotePath,
        dst: &RemotePath,
        options: &MultipartCopyOptions,
        plan: &MultipartCopyPlan,
        copy_source: &str,
        source_etag: &str,
        upload_id: String,
        cancellation: &MultipartCopyCancellation,
        on_progress: &MultipartCopyProgress<'_>,
        attributes: &ObjectAttributes,
    ) -> Result<MultipartCopyResult> {
        use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};

        let mut completed_parts = Vec::with_capacity(plan.parts.len());
        let mut bytes_copied = 0_u64;
        let mut service_source_version = None;

        for part in &plan.parts {
            let copy_request = self
                .inner
                .upload_part_copy()
                .bucket(&dst.bucket)
                .key(&dst.key)
                .upload_id(&upload_id)
                .part_number(part.part_number)
                .copy_source(copy_source)
                .copy_source_range(format!("bytes={}-{}", part.start, part.end_inclusive))
                .copy_source_if_match(source_etag)
                .send();
            let copy_response = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    return Err(self
                        .abort_multipart_upload_with_error(
                            dst,
                            &upload_id,
                            Error::Interrupted("Multipart copy was cancelled".to_string()),
                        )
                        .await);
                }
                response = copy_request => response,
            };

            let copy_response = match copy_response {
                Ok(response) => response,
                Err(error) => {
                    let primary = Self::map_object_request_error(
                        &error,
                        src,
                        options.source_version_id.as_deref(),
                    );
                    return Err(self
                        .abort_multipart_upload_with_error(dst, &upload_id, primary)
                        .await);
                }
            };

            if service_source_version.is_none() {
                service_source_version = copy_response
                    .copy_source_version_id()
                    .map(ToString::to_string);
            }
            let etag = match copy_response
                .copy_part_result()
                .and_then(|result| result.e_tag())
            {
                Some(etag) => etag.trim_matches('"').to_string(),
                None => {
                    return Err(self
                        .abort_multipart_upload_with_error(
                            dst,
                            &upload_id,
                            Error::General(format!(
                                "UploadPartCopy response for part {} did not include an ETag",
                                part.part_number
                            )),
                        )
                        .await);
                }
            };
            completed_parts.push(
                CompletedPart::builder()
                    .part_number(part.part_number)
                    .e_tag(etag)
                    .build(),
            );

            bytes_copied += part.size;
            on_progress(bytes_copied);
        }

        let completed_upload = CompletedMultipartUpload::builder()
            .set_parts(Some(completed_parts))
            .build();
        let complete_request = self
            .inner
            .complete_multipart_upload()
            .bucket(&dst.bucket)
            .key(&dst.key)
            .upload_id(&upload_id)
            .multipart_upload(completed_upload)
            .send();
        let complete_response = tokio::select! {
            biased;
            // A successful completion is irreversible. Prefer an already-ready
            // service response over a simultaneous cancellation signal so the
            // caller is not told that a completed destination was interrupted.
            response = complete_request => response,
            _ = cancellation.cancelled() => {
                return Err(self
                    .abort_multipart_upload_with_error(
                        dst,
                        &upload_id,
                        Error::Interrupted("Multipart copy was cancelled".to_string()),
                    )
                    .await);
            }
        };
        let complete_response = match complete_response {
            Ok(response) => response,
            Err(error) => {
                let primary = Self::map_object_request_error(&error, dst, None);
                return Err(self
                    .abort_multipart_upload_with_error(dst, &upload_id, primary)
                    .await);
            }
        };

        let mut object = ObjectInfo::file(&dst.key, plan.object_size as i64);
        object.etag = complete_response
            .e_tag()
            .map(|etag| etag.trim_matches('"').to_string());
        object.version_id = complete_response.version_id().map(ToString::to_string);
        object.source_version_id =
            service_source_version.or_else(|| options.source_version_id.clone());
        object.content_type = attributes.content_type.clone();
        object.metadata =
            (!attributes.user_metadata.is_empty()).then(|| attributes.user_metadata.clone());
        object.last_modified = Some(jiff::Timestamp::now());

        Ok(MultipartCopyResult {
            object,
            upload_id,
            part_count: plan.parts.len(),
            bytes_copied,
        })
    }
}

fn build_tagging(
    tags: std::collections::HashMap<String, String>,
) -> Result<aws_sdk_s3::types::Tagging> {
    use aws_sdk_s3::types::{Tag, Tagging};

    let mut tag_set = Vec::with_capacity(tags.len());
    for (key, value) in tags {
        let tag = Tag::builder()
            .key(key)
            .value(value)
            .build()
            .map_err(|e| Error::General(format!("invalid tag: {e}")))?;
        tag_set.push(tag);
    }

    Tagging::builder()
        .set_tag_set(Some(tag_set))
        .build()
        .map_err(|e| Error::General(format!("invalid tagging payload: {e}")))
}

fn validate_continuation_token(
    truncated: bool,
    current: Option<&str>,
    next: Option<&str>,
) -> Result<()> {
    if truncated && (next.is_none() || next == current) {
        return Err(Error::Network(
            "S3 returned a truncated object listing without a new continuation token".to_string(),
        ));
    }

    Ok(())
}

fn validate_multipart_upload_markers(
    truncated: bool,
    current_key_marker: Option<&str>,
    current_upload_id_marker: Option<&str>,
    next_key_marker: Option<&str>,
    next_upload_id_marker: Option<&str>,
) -> Result<()> {
    if !truncated {
        return Ok(());
    }
    if next_key_marker.is_none() || next_upload_id_marker.is_none() {
        return Err(Error::Network(
            "S3 returned a truncated multipart upload listing without a complete marker pair"
                .to_string(),
        ));
    }
    if current_key_marker == next_key_marker && current_upload_id_marker == next_upload_id_marker {
        return Err(Error::Network(
            "S3 returned a truncated multipart upload listing without advancing its markers"
                .to_string(),
        ));
    }
    Ok(())
}

fn kms_diagnostic_sdk_error<E>(error: &aws_sdk_s3::error::SdkError<E>) -> Error {
    let status = match error {
        aws_sdk_s3::error::SdkError::ServiceError(service_error) => {
            Some(service_error.raw().status().as_u16())
        }
        _ => None,
    };
    match status {
        Some(401 | 403) => Error::Auth("KMS diagnostic object permission was denied".to_string()),
        Some(404) => Error::NotFound("KMS diagnostic bucket was not found".to_string()),
        Some(409 | 412) => {
            Error::Conflict("KMS diagnostic object operation conflicted".to_string())
        }
        Some(400 | 422) => Error::General("KMS diagnostic object request was rejected".to_string()),
        _ => Error::Network("KMS diagnostic object request failed".to_string()),
    }
}

#[async_trait]
impl KmsDiagnosticStore for S3Client {
    async fn put_kms_diagnostic_object(
        &self,
        path: &RemotePath,
        content: Zeroizing<Vec<u8>>,
        key_id: &str,
    ) -> Result<()> {
        let body = aws_sdk_s3::primitives::ByteStream::from(Bytes::from_owner(
            KmsDiagnosticObjectBody(content),
        ));
        self.inner
            .put_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .body(body)
            .server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::AwsKms)
            .ssekms_key_id(key_id)
            .send()
            .await
            .map_err(|error| kms_diagnostic_sdk_error(&error))?;
        Ok(())
    }

    async fn get_kms_diagnostic_object(
        &self,
        path: &RemotePath,
        max_bytes: usize,
    ) -> Result<Zeroizing<Vec<u8>>> {
        let response = self
            .inner
            .get_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .send()
            .await
            .map_err(|error| kms_diagnostic_sdk_error(&error))?;
        if response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .is_some_and(|length| length > max_bytes)
        {
            return Err(Error::General(
                "KMS diagnostic object exceeded the bounded probe size".to_string(),
            ));
        }

        let mut content = Zeroizing::new(Vec::with_capacity(max_bytes.min(64 * 1024)));
        let mut body = response.body;
        while let Some(chunk) = body
            .try_next()
            .await
            .map_err(|_| Error::Network("Failed to read KMS diagnostic object".to_string()))?
        {
            if content.len().saturating_add(chunk.len()) > max_bytes {
                return Err(Error::General(
                    "KMS diagnostic object exceeded the bounded probe size".to_string(),
                ));
            }
            content.extend_from_slice(&chunk);
        }
        Ok(content)
    }

    async fn delete_kms_diagnostic_object(&self, path: &RemotePath) -> Result<()> {
        self.inner
            .delete_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .customize()
            .mutate_request(|request| {
                request
                    .headers_mut()
                    .insert(RUSTFS_FORCE_DELETE_HEADER, "true");
            })
            .send()
            .await
            .map_err(|error| kms_diagnostic_sdk_error(&error))?;
        Ok(())
    }
}

#[async_trait]
impl ObjectStore for S3Client {
    async fn list_buckets(&self) -> Result<Vec<ObjectInfo>> {
        let response = self.inner.list_buckets().send().await.map_err(|e| {
            let message = Self::format_sdk_error(&e);
            if let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &e
                && matches!(service_error.raw().status().as_u16(), 401 | 403)
            {
                Error::Auth(message)
            } else {
                Error::Network(message)
            }
        })?;

        let buckets = response
            .buckets()
            .iter()
            .map(|b| {
                let mut info = ObjectInfo::bucket(b.name().unwrap_or_default());
                if let Some(creation_date) = b.creation_date() {
                    info.last_modified = jiff::Timestamp::from_second(creation_date.secs()).ok();
                }
                info
            })
            .collect();

        Ok(buckets)
    }

    async fn list_objects(&self, path: &RemotePath, options: ListOptions) -> Result<ListResult> {
        let mut request = self.inner.list_objects_v2().bucket(&path.bucket);

        // Set prefix
        let prefix = if path.key.is_empty() {
            options.prefix.clone()
        } else if let Some(p) = &options.prefix {
            Some(format!("{}{}", path.key, p))
        } else {
            Some(path.key.clone())
        };

        if let Some(p) = prefix {
            request = request.prefix(p);
        }

        // Set delimiter (for non-recursive listing)
        if !options.recursive {
            request = request.delimiter(options.delimiter.as_deref().unwrap_or("/"));
        }

        // Set max keys
        if let Some(max) = options.max_keys {
            request = request.max_keys(max);
        }

        // Set continuation token
        if let Some(token) = &options.continuation_token {
            request = request.continuation_token(token);
        }

        let response = request.send().await.map_err(|e| {
            let err_str = Self::format_sdk_error(&e);
            if let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &e
                && matches!(service_error.raw().status().as_u16(), 401 | 403)
            {
                Error::Auth(err_str)
            } else if err_str.contains("NotFound") || err_str.contains("NoSuchBucket") {
                Error::NotFound(format!("Bucket not found: {}", path.bucket))
            } else {
                Error::Network(err_str)
            }
        })?;

        let mut items = Vec::new();

        // Add common prefixes (directories)
        for prefix in response.common_prefixes() {
            if let Some(p) = prefix.prefix() {
                items.push(ObjectInfo::dir(p));
            }
        }

        // Add objects
        for object in response.contents() {
            let key = object.key().unwrap_or_default().to_string();
            let size = object.size().unwrap_or(0);
            let mut info = ObjectInfo::file(&key, size);

            if let Some(modified) = object.last_modified() {
                info.last_modified = jiff::Timestamp::from_second(modified.secs()).ok();
            }

            if let Some(etag) = object.e_tag() {
                info.etag = Some(etag.trim_matches('"').to_string());
            }

            if let Some(sc) = object.storage_class() {
                info.storage_class = Some(sc.as_str().to_string());
            }

            items.push(info);
        }

        let truncated = response.is_truncated().unwrap_or(false);
        let continuation_token = response.next_continuation_token().map(ToString::to_string);
        validate_continuation_token(
            truncated,
            options.continuation_token.as_deref(),
            continuation_token.as_deref(),
        )?;

        Ok(ListResult {
            items,
            truncated,
            continuation_token,
        })
    }

    async fn list_multipart_uploads(
        &self,
        bucket: &str,
        options: MultipartUploadListOptions,
    ) -> Result<MultipartUploadListResult> {
        let mut request = self.inner.list_multipart_uploads().bucket(bucket);
        if let Some(prefix) = &options.prefix {
            request = request.prefix(prefix);
        }
        if let Some(delimiter) = &options.delimiter {
            request = request.delimiter(delimiter);
        }
        if let Some(key_marker) = &options.key_marker {
            request = request.key_marker(key_marker);
        }
        if let Some(upload_id_marker) = &options.upload_id_marker {
            request = request.upload_id_marker(upload_id_marker);
        }
        if let Some(max_uploads) = options.max_uploads {
            request = request.max_uploads(max_uploads);
        }

        let response = request.send().await.map_err(|error| {
            let message = Self::format_sdk_error(&error);
            if let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &error {
                let status = service_error.raw().status().as_u16();
                let code = service_error.err().code();
                if matches!(status, 401 | 403) {
                    return Error::Auth(message);
                }
                if status == 501 || matches!(code, Some("NotImplemented")) {
                    return Error::UnsupportedFeature(message);
                }
                if status == 404 || matches!(code, Some("NoSuchBucket") | Some("NotFound")) {
                    return Error::NotFound(format!("Bucket not found: {bucket}"));
                }
                return Error::Network(format!("{message} (HTTP {status})"));
            }
            Error::Network(message)
        })?;

        let truncated = response.is_truncated().unwrap_or(false);
        let uploads = response
            .uploads()
            .iter()
            .map(|upload| {
                let key = upload.key().ok_or_else(|| {
                    Error::Network(
                        "S3 returned a multipart upload without an object key".to_string(),
                    )
                })?;
                let upload_id = upload.upload_id().ok_or_else(|| {
                    Error::Network(
                        "S3 returned a multipart upload without an upload ID".to_string(),
                    )
                })?;
                Ok(MultipartUpload {
                    bucket: bucket.to_string(),
                    key: key.to_string(),
                    upload_id: upload_id.to_string(),
                    initiated: upload.initiated().and_then(|value| {
                        Timestamp::new(value.secs(), value.subsec_nanos() as i32).ok()
                    }),
                    size_bytes: None,
                    storage_class: upload
                        .storage_class()
                        .map(|value| value.as_str().to_string()),
                    initiator: upload.initiator().map(|identity| MultipartIdentity {
                        id: identity.id().map(ToString::to_string),
                        display_name: identity.display_name().map(ToString::to_string),
                    }),
                    owner: upload.owner().map(|identity| MultipartIdentity {
                        id: identity.id().map(ToString::to_string),
                        display_name: identity.display_name().map(ToString::to_string),
                    }),
                    checksum_algorithm: upload
                        .checksum_algorithm()
                        .map(|value| value.as_str().to_string()),
                    checksum_type: upload
                        .checksum_type()
                        .map(|value| value.as_str().to_string()),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let next_key_marker = response.next_key_marker().map(ToString::to_string);
        let next_upload_id_marker = response.next_upload_id_marker().map(ToString::to_string);
        validate_multipart_upload_markers(
            truncated,
            options.key_marker.as_deref(),
            options.upload_id_marker.as_deref(),
            next_key_marker.as_deref(),
            next_upload_id_marker.as_deref(),
        )?;
        let common_prefixes = response
            .common_prefixes()
            .iter()
            .filter_map(|prefix| prefix.prefix().map(ToString::to_string))
            .collect();

        Ok(MultipartUploadListResult {
            uploads,
            common_prefixes,
            truncated,
            next_key_marker,
            next_upload_id_marker,
        })
    }

    async fn abort_multipart_upload(&self, request: &AbortMultipartUploadRequest) -> Result<()> {
        let result = self
            .inner
            .abort_multipart_upload()
            .bucket(&request.bucket)
            .key(&request.key)
            .upload_id(&request.upload_id)
            .send()
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(error) => {
                let message = Self::format_sdk_error(&error);
                if let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &error {
                    let status = service_error.raw().status().as_u16();
                    let code = service_error.err().code();
                    if matches!(status, 401 | 403) {
                        return Err(Error::Auth(message));
                    }
                    if status == 404 && matches!(code, Some("NoSuchUpload")) {
                        return Ok(());
                    }
                    if status == 501 || matches!(code, Some("NotImplemented")) {
                        return Err(Error::UnsupportedFeature(message));
                    }
                    if status == 404 || matches!(code, Some("NoSuchBucket") | Some("NotFound")) {
                        return Err(Error::NotFound(format!(
                            "Multipart upload target not found: {}/{}",
                            request.bucket, request.key
                        )));
                    }
                    return Err(Error::Network(format!("{message} (HTTP {status})")));
                }
                Err(Error::Network(message))
            }
        }
    }

    async fn head_object(&self, path: &RemotePath) -> Result<ObjectInfo> {
        self.head_object_with_options(path, &ObjectReadOptions::default())
            .await
    }

    async fn head_object_with_options(
        &self,
        path: &RemotePath,
        options: &ObjectReadOptions,
    ) -> Result<ObjectInfo> {
        let mut request = self.inner.head_object().bucket(&path.bucket).key(&path.key);
        if let Some(version_id) = &options.version_id {
            request = request.version_id(version_id);
        }
        let response = request.send().await.map_err(|error| {
            Self::map_object_request_error(&error, path, options.version_id.as_deref())
        })?;

        if response.delete_marker().unwrap_or(false) {
            return Err(Error::DeleteMarker {
                path: path.to_string(),
                version_id: response
                    .version_id()
                    .or(options.version_id.as_deref())
                    .unwrap_or("unknown")
                    .to_string(),
            });
        }

        let size = response.content_length().unwrap_or(0);
        let mut info = ObjectInfo::file(&path.key, size);

        if let Some(modified) = response.last_modified() {
            info.last_modified = jiff::Timestamp::from_second(modified.secs()).ok();
        }

        if let Some(etag) = response.e_tag() {
            info.etag = Some(etag.trim_matches('"').to_string());
        }

        if let Some(ct) = response.content_type() {
            info.content_type = Some(ct.to_string());
        }

        if let Some(sc) = response.storage_class() {
            info.storage_class = Some(sc.as_str().to_string());
        }

        if let Some(meta) = response.metadata()
            && !meta.is_empty()
        {
            info.metadata = Some(meta.clone());
        }
        info.version_id = response
            .version_id()
            .or(options.version_id.as_deref())
            .map(ToString::to_string);

        Ok(info)
    }

    async fn head_object_with_transfer_options(
        &self,
        path: &RemotePath,
        options: &TransferReadOptions,
    ) -> Result<ObjectInfo> {
        options.validate()?;
        self.ensure_sse_customer_transport(options.customer_key.as_ref())?;
        let mut request = apply_sse_customer_to_head_request(
            self.inner.head_object().bucket(&path.bucket).key(&path.key),
            options.customer_key.as_ref(),
        );
        if let Some(version_id) = &options.version_id {
            request = request.version_id(version_id);
        }
        if options.checksum_mode {
            request = request.checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled);
        }
        let response = request.send().await.map_err(|error| {
            self.redact_sse_customer_error(
                Self::map_object_request_error(&error, path, options.version_id.as_deref()),
                options.customer_key.as_ref(),
            )
        })?;
        if response.delete_marker().unwrap_or(false) {
            return Err(Error::DeleteMarker {
                path: path.to_string(),
                version_id: response
                    .version_id()
                    .or(options.version_id.as_deref())
                    .unwrap_or("unknown")
                    .to_string(),
            });
        }

        let mut info = ObjectInfo::file(&path.key, response.content_length().unwrap_or(0));
        info.last_modified = response
            .last_modified()
            .and_then(|value| jiff::Timestamp::from_second(value.secs()).ok());
        info.etag = response
            .e_tag()
            .map(|value| value.trim_matches('"').to_string());
        info.content_type = response.content_type().map(ToString::to_string);
        info.storage_class = response
            .storage_class()
            .map(|value| value.as_str().to_string());
        info.metadata = response
            .metadata()
            .filter(|metadata| !metadata.is_empty())
            .cloned();
        info.version_id = response
            .version_id()
            .or(options.version_id.as_deref())
            .map(ToString::to_string);
        Ok(info)
    }

    async fn head_object_transfer_metadata(
        &self,
        path: &RemotePath,
        options: &TransferReadOptions,
    ) -> Result<ObjectTransferMetadata> {
        options.validate()?;
        self.ensure_sse_customer_transport(options.customer_key.as_ref())?;
        let mut request = apply_sse_customer_to_head_request(
            self.inner.head_object().bucket(&path.bucket).key(&path.key),
            options.customer_key.as_ref(),
        );
        if let Some(version_id) = &options.version_id {
            request = request.version_id(version_id);
        }
        if options.checksum_mode {
            request = request.checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled);
        }
        let response = request.send().await.map_err(|error| {
            self.redact_sse_customer_error(
                Self::map_object_request_error(&error, path, options.version_id.as_deref()),
                options.customer_key.as_ref(),
            )
        })?;
        if response.delete_marker().unwrap_or(false) {
            return Err(Error::DeleteMarker {
                path: path.to_string(),
                version_id: response
                    .version_id()
                    .or(options.version_id.as_deref())
                    .unwrap_or("unknown")
                    .to_string(),
            });
        }

        let expires = response.expires_string().map(core_http_date).transpose()?;
        let checksums = if options.checksum_mode {
            response
                .checksum_sha256()
                .map(|value| persisted_sha256_checksum(value, response.checksum_type()))
                .transpose()?
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
        Ok(ObjectTransferMetadata {
            attributes: ObjectAttributes {
                content_type: response.content_type().map(ToString::to_string),
                cache_control: response.cache_control().map(ToString::to_string),
                content_disposition: response.content_disposition().map(ToString::to_string),
                content_encoding: response.content_encoding().map(ToString::to_string),
                content_language: response.content_language().map(ToString::to_string),
                expires,
                user_metadata: response.metadata().cloned().unwrap_or_default(),
            },
            storage_class: response
                .storage_class()
                .map(|storage_class| storage_class.as_str().to_string()),
            checksums,
        })
    }

    async fn bucket_exists(&self, bucket: &str) -> Result<bool> {
        match self.inner.head_bucket().bucket(bucket).send().await {
            Ok(_) => Ok(true),
            Err(e) => {
                // Check HTTP status code for 404 first to avoid unnecessary string formatting
                // Some S3-compatible services may return 404 without standard error codes
                if let aws_sdk_s3::error::SdkError::ServiceError(ref service_err) = e
                    && service_err.raw().status().as_u16() == 404
                {
                    return Ok(false);
                }
                let err_str = e.to_string();
                if err_str.contains("NotFound") || err_str.contains("NoSuchBucket") {
                    return Ok(false);
                }
                Err(Error::Network(err_str))
            }
        }
    }

    async fn create_bucket(&self, bucket: &str) -> Result<()> {
        ObjectStore::create_bucket_with_options(self, bucket, &CreateBucketOptions::default()).await
    }

    async fn create_bucket_with_options(
        &self,
        bucket: &str,
        options: &CreateBucketOptions,
    ) -> Result<()> {
        use aws_sdk_s3::types::{BucketLocationConstraint, CreateBucketConfiguration};

        options.validate()?;
        let mut request = self.inner.create_bucket().bucket(bucket);
        if let Some(region) = &options.region {
            let configuration = CreateBucketConfiguration::builder()
                .location_constraint(BucketLocationConstraint::from(region.as_str()))
                .build();
            request = request.create_bucket_configuration(configuration);
        }
        if options.object_lock_enabled {
            request = request.object_lock_enabled_for_bucket(true);
        }

        request.send().await.map_err(|error| {
            let formatted = Self::format_sdk_error(&error);
            let mapped = if let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &error {
                let status = service_error.raw().status().as_u16();
                let code = service_error.err().code();
                if matches!(status, 401 | 403)
                    || matches!(
                        code,
                        Some("AccessDenied")
                            | Some("InvalidAccessKeyId")
                            | Some("Forbidden")
                            | Some("Unauthorized")
                    )
                {
                    Error::Auth(formatted)
                } else if status == 409
                    || matches!(
                        code,
                        Some("BucketAlreadyExists") | Some("BucketAlreadyOwnedByYou")
                    )
                {
                    Error::Conflict(formatted)
                } else {
                    Error::Network(formatted)
                }
            } else {
                Error::Network(formatted)
            };
            self.redact_sensitive_error(mapped)
        })?;

        Ok(())
    }

    async fn get_bucket_location(&self, bucket: &str) -> Result<Option<String>> {
        self.inner
            .get_bucket_location()
            .bucket(bucket)
            .send()
            .await
            .map(|response| {
                response
                    .location_constraint()
                    .map(|location| location.as_str().to_string())
            })
            .map_err(|error| {
                let formatted = Self::format_sdk_error(&error);
                let mapped =
                    if let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &error {
                        let status = service_error.raw().status().as_u16();
                        if matches!(status, 401 | 403) {
                            Error::Auth(formatted)
                        } else if status == 404 {
                            Error::NotFound(format!("Bucket not found: {bucket}"))
                        } else {
                            Error::Network(formatted)
                        }
                    } else {
                        Error::Network(formatted)
                    };
                self.redact_sensitive_error(mapped)
            })
    }

    async fn delete_bucket(&self, bucket: &str) -> Result<()> {
        self.inner
            .delete_bucket()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| {
                let err_str = Self::format_sdk_error(&e);
                let mapped = if let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &e {
                    let status = service_error.raw().status().as_u16();
                    let code = service_error.err().code();
                    if matches!(status, 401 | 403)
                        || matches!(
                            code,
                            Some("AccessDenied") | Some("Forbidden") | Some("Unauthorized")
                        )
                    {
                        Error::Auth(err_str)
                    } else if status == 404
                        || matches!(code, Some("NotFound") | Some("NoSuchBucket"))
                    {
                        Error::NotFound(format!("Bucket not found: {bucket}"))
                    } else if status == 409 || matches!(code, Some("BucketNotEmpty")) {
                        Error::Conflict(err_str)
                    } else {
                        Error::Network(err_str)
                    }
                } else if err_str.contains("NotFound") || err_str.contains("NoSuchBucket") {
                    Error::NotFound(format!("Bucket not found: {bucket}"))
                } else if err_str.contains("BucketNotEmpty") {
                    Error::Conflict(err_str)
                } else {
                    Error::Network(err_str)
                };
                self.redact_sensitive_error(mapped)
            })?;

        Ok(())
    }

    async fn capabilities(&self) -> Result<Capabilities> {
        // Best-effort hints for common S3-compatible backends. `select` is not inferred here
        // because `rc sql` determines support from the real request result.
        Ok(Capabilities {
            versioning: true,
            object_lock: true,
            tagging: true,
            anonymous: true,
            select: false,
            notifications: true,
            lifecycle: true,
            replication: true,
            cors: true,
        })
    }

    async fn get_object(&self, path: &RemotePath) -> Result<Vec<u8>> {
        self.get_object_with_progress(path, |_, _| {}).await
    }

    async fn get_object_with_options(
        &self,
        path: &RemotePath,
        options: &ObjectReadOptions,
    ) -> Result<Vec<u8>> {
        self.get_object_with_progress_and_options(path, options, |_, _| {})
            .await
    }

    async fn get_object_with_transfer_options(
        &self,
        path: &RemotePath,
        options: &TransferReadOptions,
    ) -> Result<Vec<u8>> {
        options.validate()?;
        self.ensure_sse_customer_transport(options.customer_key.as_ref())?;
        let mut request = apply_sse_customer_to_get_request(
            self.inner.get_object().bucket(&path.bucket).key(&path.key),
            options.customer_key.as_ref(),
        );
        if let Some(version_id) = &options.version_id {
            request = request.version_id(version_id);
        }
        if options.checksum_mode {
            request = request.checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled);
        }
        let response = request.send().await.map_err(|error| {
            self.redact_sse_customer_error(
                Self::map_object_request_error(&error, path, options.version_id.as_deref()),
                options.customer_key.as_ref(),
            )
        })?;
        if response.delete_marker().unwrap_or(false) {
            return Err(Error::DeleteMarker {
                path: path.to_string(),
                version_id: response
                    .version_id()
                    .or(options.version_id.as_deref())
                    .unwrap_or("unknown")
                    .to_string(),
            });
        }
        let mut data = Vec::new();
        let mut body = response.body;
        while let Some(chunk) = body.try_next().await.map_err(|error| {
            self.redact_sse_customer_error(
                Error::Network(error.to_string()),
                options.customer_key.as_ref(),
            )
        })? {
            data.extend_from_slice(&chunk);
        }
        Ok(data)
    }

    async fn write_object_to_with_options(
        &self,
        path: &RemotePath,
        options: &ObjectReadOptions,
        writer: &mut (dyn AsyncWrite + Send + Unpin),
        max_bytes: Option<u64>,
    ) -> Result<u64> {
        S3Client::write_object_to_with_options(self, path, options, writer, max_bytes).await
    }

    async fn write_object_to_with_transfer_options(
        &self,
        path: &RemotePath,
        options: &TransferReadOptions,
        writer: &mut (dyn AsyncWrite + Send + Unpin),
        max_bytes: Option<u64>,
    ) -> Result<u64> {
        options.validate()?;
        self.ensure_sse_customer_transport(options.customer_key.as_ref())?;
        if matches!(max_bytes, Some(0)) {
            self.head_object_with_transfer_options(path, options)
                .await?;
            return Ok(0);
        }
        let mut request = apply_sse_customer_to_get_request(
            self.inner.get_object().bucket(&path.bucket).key(&path.key),
            options.customer_key.as_ref(),
        );
        if let Some(version_id) = &options.version_id {
            request = request.version_id(version_id);
        }
        if options.checksum_mode {
            request = request.checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled);
        }
        if let Some(max_bytes) = max_bytes {
            request = request.range(format!("bytes=0-{}", max_bytes - 1));
        }
        let response = request.send().await.map_err(|error| {
            self.redact_sse_customer_error(
                Self::map_object_request_error(&error, path, options.version_id.as_deref()),
                options.customer_key.as_ref(),
            )
        })?;
        let mut body = response.body;
        let mut bytes_written = 0u64;
        while let Some(chunk) = body.try_next().await.map_err(|error| {
            self.redact_sse_customer_error(
                Error::Network(error.to_string()),
                options.customer_key.as_ref(),
            )
        })? {
            let remaining = max_bytes
                .map(|limit| limit.saturating_sub(bytes_written))
                .unwrap_or(chunk.len() as u64);
            let write_len = chunk.len().min(remaining as usize);
            writer.write_all(&chunk[..write_len]).await?;
            bytes_written += write_len as u64;
            if max_bytes.is_some_and(|limit| bytes_written >= limit) {
                break;
            }
        }
        writer.flush().await?;
        Ok(bytes_written)
    }

    async fn put_object(
        &self,
        path: &RemotePath,
        data: Vec<u8>,
        content_type: Option<&str>,
        encryption: Option<&ObjectEncryptionRequest>,
    ) -> Result<ObjectInfo> {
        let size = data.len() as i64;
        let body = aws_sdk_s3::primitives::ByteStream::from(data);

        let mut request = apply_object_encryption_to_put_request(
            self.inner
                .put_object()
                .bucket(&path.bucket)
                .key(&path.key)
                .body(body),
            encryption,
        );

        if let Some(ct) = content_type {
            request = request.content_type(ct);
        }

        let response = request
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;

        let mut info = ObjectInfo::file(&path.key, size);
        if let Some(etag) = response.e_tag() {
            info.etag = Some(etag.trim_matches('"').to_string());
        }
        info.version_id = response.version_id().map(ToString::to_string);
        info.last_modified = Some(jiff::Timestamp::now());

        Ok(info)
    }

    async fn put_object_with_options(
        &self,
        path: &RemotePath,
        data: Vec<u8>,
        options: &ObjectWriteOptions,
    ) -> Result<ObjectInfo> {
        validate_attribute_tag_write_options(options)?;
        self.ensure_write_sse_customer_transport(options.encryption.as_ref())?;
        let size = data.len() as i64;
        let requested_checksum = requested_sha256_checksum(&data, options.checksum.as_ref())?;
        let body = aws_sdk_s3::primitives::ByteStream::from(data);
        let storage_class = rustfs_storage_class(options.storage_class.as_deref())?;
        let request = apply_object_write_encryption_to_put_request(
            self.inner
                .put_object()
                .bucket(&path.bucket)
                .key(&path.key)
                .body(body),
            options.encryption.as_ref(),
        )
        .set_storage_class(storage_class);
        let mut request =
            apply_object_attributes_to_put_request(request, options.attributes.as_ref())?;
        if let Some(tags) = &options.tags {
            request = request.tagging(encode_object_tags(tags));
        }
        if let Some(checksum) = &requested_checksum {
            request = request
                .checksum_algorithm(aws_sdk_s3::types::ChecksumAlgorithm::Sha256)
                .checksum_sha256(checksum);
        }
        request = apply_object_lock_to_put_request(request, options)?;
        let response = request.send().await.map_err(|error| {
            self.redact_sse_customer_error(
                Self::map_transfer_write_error(&error, path, options),
                destination_sse_customer_key(options.encryption.as_ref()),
            )
        })?;

        let mut info = ObjectInfo::file(&path.key, size);
        if let Some(attributes) = &options.attributes {
            info.content_type = attributes.content_type.clone();
            info.metadata =
                (!attributes.user_metadata.is_empty()).then(|| attributes.user_metadata.clone());
        }
        info.storage_class = options.storage_class.clone();
        info.etag = response
            .e_tag()
            .map(|etag| etag.trim_matches('"').to_string());
        info.version_id = response.version_id().map(ToString::to_string);
        info.last_modified = Some(jiff::Timestamp::now());
        if let Some(requested_checksum) = requested_checksum {
            self.verify_persisted_sha256(
                path,
                info.version_id.clone(),
                &requested_checksum,
                destination_sse_customer_key(options.encryption.as_ref()),
            )
            .await?;
        }
        Ok(info)
    }

    async fn delete_object(&self, path: &RemotePath) -> Result<()> {
        S3Client::delete_object_with_options(self, path, DeleteRequestOptions::default()).await
    }

    async fn delete_object_with_options(
        &self,
        path: &RemotePath,
        options: DeleteRequestOptions,
    ) -> Result<DeletedObject> {
        self.delete_object_with_result(path, options).await
    }

    async fn delete_objects(&self, bucket: &str, keys: Vec<String>) -> Result<Vec<String>> {
        self.delete_objects_with_options(bucket, keys, DeleteRequestOptions::default())
            .await
    }

    async fn delete_object_versions(
        &self,
        bucket: &str,
        objects: Vec<ObjectVersionIdentifier>,
        options: DeleteRequestOptions,
    ) -> Result<DeleteObjectsResult> {
        self.delete_object_versions_with_options(bucket, objects, options)
            .await
    }

    async fn copy_object(
        &self,
        src: &RemotePath,
        dst: &RemotePath,
        encryption: Option<&ObjectEncryptionRequest>,
    ) -> Result<ObjectInfo> {
        self.copy_object_with_options(src, dst, &CopyObjectOptions::default(), encryption)
            .await
    }

    async fn copy_object_with_options(
        &self,
        src: &RemotePath,
        dst: &RemotePath,
        options: &CopyObjectOptions,
        encryption: Option<&ObjectEncryptionRequest>,
    ) -> Result<ObjectInfo> {
        let copy_source = encoded_copy_source(src, options.source_version_id.as_deref());
        let response = apply_object_encryption_to_copy_request(
            self.inner
                .copy_object()
                .copy_source(&copy_source)
                .bucket(&dst.bucket)
                .key(&dst.key),
            encryption,
        )
        .send()
        .await
        .map_err(|error| {
            Self::map_object_request_error(&error, src, options.source_version_id.as_deref())
        })?;

        // Get size from head_object since copy doesn't return it
        let info = self.head_object(dst).await?;

        // Update etag from copy response if available
        let mut result = info;
        if let Some(version_id) = response.version_id() {
            result.version_id = Some(version_id.to_string());
        }
        result.source_version_id = response.copy_source_version_id().map(ToString::to_string);
        if let Some(copy_result) = response.copy_object_result()
            && let Some(etag) = copy_result.e_tag()
        {
            result.etag = Some(etag.trim_matches('"').to_string());
        }

        Ok(result)
    }

    async fn copy_object_with_transfer_options(
        &self,
        src: &RemotePath,
        dst: &RemotePath,
        options: &TransferCopyOptions,
    ) -> Result<ObjectInfo> {
        validate_beta10_copy_options(options)?;
        let copy_source = encoded_copy_source(src, options.source.version_id.as_deref());
        let encryption = managed_object_encryption(&options.destination)?;
        let storage_class = rustfs_storage_class(options.destination.storage_class.as_deref())?;
        let mut request = apply_object_encryption_to_copy_request(
            self.inner
                .copy_object()
                .copy_source(&copy_source)
                .bucket(&dst.bucket)
                .key(&dst.key),
            encryption,
        )
        .set_storage_class(storage_class);
        if matches!(options.metadata_directive, Some(MetadataDirective::Copy)) {
            request = request.metadata_directive(aws_sdk_s3::types::MetadataDirective::Copy);
        }
        request = apply_object_lock_to_copy_request(request, &options.destination)?;
        let response = request.send().await.map_err(|error| {
            if options.destination.retention.is_some() || options.destination.legal_hold.is_some() {
                Self::map_object_lock_write_error(
                    &error,
                    dst,
                    src,
                    options.source.version_id.as_deref(),
                )
            } else {
                Self::map_object_request_error(&error, src, options.source.version_id.as_deref())
            }
        })?;

        let mut result = self.head_object(dst).await?;
        result.version_id = response
            .version_id()
            .map(ToString::to_string)
            .or(result.version_id);
        result.source_version_id = response.copy_source_version_id().map(ToString::to_string);
        if let Some(etag) = response
            .copy_object_result()
            .and_then(|copy_result| copy_result.e_tag())
        {
            result.etag = Some(etag.trim_matches('"').to_string());
        }
        Ok(result)
    }

    async fn multipart_copy(
        &self,
        src: &RemotePath,
        dst: &RemotePath,
        options: &MultipartCopyOptions,
        cancellation: &MultipartCopyCancellation,
        encryption: Option<&ObjectEncryptionRequest>,
        on_progress: &MultipartCopyProgress<'_>,
    ) -> Result<MultipartCopyResult> {
        if cancellation.is_cancelled() {
            return Err(self.redact_sensitive_error(Error::Interrupted(
                "Multipart copy cancelled before upload creation".to_string(),
            )));
        }

        let plan = options
            .plan()
            .map_err(|error| self.redact_sensitive_error(error))?;
        let copy_source = encoded_copy_source(src, options.source_version_id.as_deref());
        let source_etag = quoted_etag(&options.source_etag);
        let attributes = ObjectAttributes {
            content_type: options.content_type.clone(),
            user_metadata: options.metadata.clone(),
            ..ObjectAttributes::default()
        };

        let create_request = apply_object_encryption_to_multipart_create_request(
            self.inner
                .create_multipart_upload()
                .bucket(&dst.bucket)
                .key(&dst.key),
            encryption,
        );
        let create_request =
            apply_object_attributes_to_multipart_create_request(create_request, &attributes)?;
        let create_response = create_request.send().await.map_err(|error| {
            self.redact_sensitive_error(Self::map_object_request_error(&error, dst, None))
        })?;
        let upload_id = create_response
            .upload_id()
            .ok_or_else(|| {
                self.redact_sensitive_error(Error::General(
                    "Multipart copy create response did not include an upload ID".to_string(),
                ))
            })?
            .to_string();

        self.finish_multipart_copy(
            src,
            dst,
            options,
            &plan,
            &copy_source,
            &source_etag,
            upload_id,
            cancellation,
            on_progress,
            &attributes,
        )
        .await
    }

    async fn multipart_copy_with_transfer_options(
        &self,
        src: &RemotePath,
        dst: &RemotePath,
        multipart: &MultipartCopyOptions,
        transfer: &TransferCopyOptions,
        cancellation: &MultipartCopyCancellation,
        on_progress: &MultipartCopyProgress<'_>,
    ) -> Result<MultipartCopyResult> {
        if cancellation.is_cancelled() {
            return Err(self.redact_sensitive_error(Error::Interrupted(
                "Multipart copy cancelled before metadata preflight".to_string(),
            )));
        }
        validate_beta10_copy_options(transfer)?;
        if transfer.destination.storage_class.is_some() {
            return Err(Error::UnsupportedFeature(
                "RustFS beta.10 does not persist storage class for multipart uploads; tracked by rustfs/backlog#1464"
                    .to_string(),
            ));
        }
        if transfer.source.version_id.is_some()
            && transfer.source.version_id.as_deref() != multipart.source_version_id.as_deref()
        {
            return Err(Error::InvalidPath(
                "Transfer and multipart source version IDs must match".to_string(),
            ));
        }
        let plan = multipart
            .plan()
            .map_err(|error| self.redact_sensitive_error(error))?;
        let attributes = if matches!(transfer.metadata_directive, Some(MetadataDirective::Copy)) {
            let mut source_options = transfer.source.clone();
            if source_options.version_id.is_none() {
                source_options.version_id = multipart.source_version_id.clone();
            }
            self.head_object_transfer_metadata(src, &source_options)
                .await?
                .attributes
        } else {
            ObjectAttributes {
                content_type: multipart.content_type.clone(),
                user_metadata: multipart.metadata.clone(),
                ..ObjectAttributes::default()
            }
        };
        if cancellation.is_cancelled() {
            return Err(self.redact_sensitive_error(Error::Interrupted(
                "Multipart copy cancelled after metadata preflight".to_string(),
            )));
        }
        let encryption = managed_object_encryption(&transfer.destination)?;
        let create_request = apply_object_encryption_to_multipart_create_request(
            self.inner
                .create_multipart_upload()
                .bucket(&dst.bucket)
                .key(&dst.key),
            encryption,
        );
        let create_request =
            apply_object_attributes_to_multipart_create_request(create_request, &attributes)?;
        let create_request =
            apply_object_lock_to_multipart_create_request(create_request, &transfer.destination)?;
        let create_response = create_request.send().await.map_err(|error| {
            self.redact_sensitive_error(Self::map_transfer_write_error(
                &error,
                dst,
                &transfer.destination,
            ))
        })?;
        let upload_id = create_response
            .upload_id()
            .ok_or_else(|| {
                self.redact_sensitive_error(Error::General(
                    "Multipart copy create response did not include an upload ID".to_string(),
                ))
            })?
            .to_string();
        let copy_source = encoded_copy_source(src, multipart.source_version_id.as_deref());
        let source_etag = quoted_etag(&multipart.source_etag);
        self.finish_multipart_copy(
            src,
            dst,
            multipart,
            &plan,
            &copy_source,
            &source_etag,
            upload_id,
            cancellation,
            on_progress,
            &attributes,
        )
        .await
    }

    async fn presign_get(&self, path: &RemotePath, expires_secs: u64) -> Result<String> {
        let config = aws_sdk_s3::presigning::PresigningConfig::builder()
            .expires_in(std::time::Duration::from_secs(expires_secs))
            .build()
            .map_err(|e| Error::General(format!("presign_get config: {e}")))?;

        let request = self
            .presign_inner
            .get_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .presigned(config)
            .await
            .map_err(|e| Error::General(format!("presign_get: {e}")))?;

        Ok(request.uri().to_string())
    }

    async fn presign_put(
        &self,
        path: &RemotePath,
        expires_secs: u64,
        content_type: Option<&str>,
    ) -> Result<String> {
        let config = aws_sdk_s3::presigning::PresigningConfig::builder()
            .expires_in(std::time::Duration::from_secs(expires_secs))
            .build()
            .map_err(|e| Error::General(format!("presign_put config: {e}")))?;

        let mut builder = self
            .presign_inner
            .put_object()
            .bucket(&path.bucket)
            .key(&path.key);

        if let Some(ct) = content_type {
            builder = builder.content_type(ct);
        }

        let request = builder
            .presigned(config)
            .await
            .map_err(|e| Error::General(format!("presign_put: {e}")))?;

        Ok(request.uri().to_string())
    }

    async fn get_versioning(&self, bucket: &str) -> Result<Option<bool>> {
        let response = self
            .inner
            .get_bucket_versioning()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| Error::General(format!("get_versioning: {e}")))?;

        Ok(response
            .status()
            .map(|s| *s == aws_sdk_s3::types::BucketVersioningStatus::Enabled))
    }

    async fn set_versioning(&self, bucket: &str, enabled: bool) -> Result<()> {
        use aws_sdk_s3::types::{BucketVersioningStatus, VersioningConfiguration};

        let status = if enabled {
            BucketVersioningStatus::Enabled
        } else {
            BucketVersioningStatus::Suspended
        };

        let config = VersioningConfiguration::builder().status(status).build();

        self.inner
            .put_bucket_versioning()
            .bucket(bucket)
            .versioning_configuration(config)
            .send()
            .await
            .map_err(|e| Error::General(format!("set_versioning: {e}")))?;

        Ok(())
    }

    async fn get_bucket_object_lock_configuration(
        &self,
        bucket: &str,
    ) -> Result<Option<BucketObjectLockConfiguration>> {
        let response = match self
            .inner
            .get_object_lock_configuration()
            .bucket(bucket)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                if let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &error {
                    let raw = service_error.raw();
                    let code = service_error.err().code().or_else(|| {
                        raw.headers()
                            .get("x-amz-error-code")
                            .and_then(|value| std::str::from_utf8(value.as_bytes()).ok())
                    });
                    if raw.status().as_u16() == 404
                        && matches!(
                            code,
                            Some("ObjectLockConfigurationNotFoundError")
                                | Some("NoSuchObjectLockConfiguration")
                        )
                    {
                        return Ok(None);
                    }
                }
                return Err(self.redact_object_lock_service_error(
                    &error,
                    Self::map_bucket_object_lock_error(&error, bucket),
                ));
            }
        };

        response
            .object_lock_configuration()
            .map(core_bucket_lock_configuration)
            .transpose()
            .map_err(|error| self.redact_sensitive_error(error))
    }

    async fn put_bucket_object_lock_configuration(
        &self,
        bucket: &str,
        configuration: BucketObjectLockConfiguration,
    ) -> Result<()> {
        let configuration = sdk_bucket_lock_configuration(configuration)?;
        self.inner
            .put_object_lock_configuration()
            .bucket(bucket)
            .object_lock_configuration(configuration)
            .send()
            .await
            .map_err(|error| {
                self.redact_object_lock_service_error(
                    &error,
                    Self::map_bucket_object_lock_error(&error, bucket),
                )
            })?;
        Ok(())
    }

    async fn get_object_retention(
        &self,
        path: &RemotePath,
        options: &ObjectLockOptions,
    ) -> Result<Option<ObjectRetention>> {
        let mut request = self
            .inner
            .get_object_retention()
            .bucket(&path.bucket)
            .key(&path.key);
        if let Some(version_id) = &options.version_id {
            request = request.version_id(version_id);
        }
        let response = request.send().await.map_err(|error| {
            self.redact_object_lock_service_error(
                &error,
                Self::map_object_request_error(&error, path, options.version_id.as_deref()),
            )
        })?;
        let Some(retention) = response.retention() else {
            return Ok(None);
        };
        let mode = retention.mode().filter(|mode| !mode.as_str().is_empty());
        let retain_until = retention.retain_until_date();
        let result = match (mode, retain_until) {
            (None, None) => Ok(None),
            (Some(mode), Some(retain_until)) => core_retention_mode(mode).and_then(|mode| {
                core_timestamp(retain_until)
                    .map(|retain_until| Some(ObjectRetention { mode, retain_until }))
            }),
            _ => Err(Error::General(
                "Object retention response must contain both Mode and RetainUntilDate".to_string(),
            )),
        };
        result.map_err(|error| self.redact_sensitive_error(error))
    }

    async fn put_object_retention(
        &self,
        path: &RemotePath,
        retention: Option<ObjectRetention>,
        options: &ObjectLockOptions,
    ) -> Result<()> {
        let retention = match retention {
            Some(retention) => aws_sdk_s3::types::ObjectLockRetention::builder()
                .mode(sdk_retention_mode(retention.mode))
                .retain_until_date(sdk_timestamp(retention.retain_until)?)
                .build(),
            None => aws_sdk_s3::types::ObjectLockRetention::builder().build(),
        };
        let mut request = self
            .inner
            .put_object_retention()
            .bucket(&path.bucket)
            .key(&path.key)
            .retention(retention);
        if let Some(version_id) = &options.version_id {
            request = request.version_id(version_id);
        }
        if options.bypass_governance {
            request = request.bypass_governance_retention(true);
        }
        request.send().await.map_err(|error| {
            self.redact_object_lock_service_error(
                &error,
                Self::map_object_request_error(&error, path, options.version_id.as_deref()),
            )
        })?;
        Ok(())
    }

    async fn get_object_legal_hold(
        &self,
        path: &RemotePath,
        options: &ObjectLockOptions,
    ) -> Result<LegalHoldStatus> {
        let mut request = self
            .inner
            .get_object_legal_hold()
            .bucket(&path.bucket)
            .key(&path.key);
        if let Some(version_id) = &options.version_id {
            request = request.version_id(version_id);
        }
        let response = request.send().await.map_err(|error| {
            self.redact_object_lock_service_error(
                &error,
                Self::map_object_request_error(&error, path, options.version_id.as_deref()),
            )
        })?;
        let status = response
            .legal_hold()
            .and_then(|legal_hold| legal_hold.status())
            .map(|status| status.as_str());
        let result = match status {
            Some("ON") => Ok(LegalHoldStatus::On),
            Some("OFF") => Ok(LegalHoldStatus::Off),
            Some(value) => Err(Error::General(format!(
                "Unsupported object legal-hold status '{value}'"
            ))),
            None => Err(Error::General(
                "Object legal-hold response is missing its status".to_string(),
            )),
        };
        result.map_err(|error| self.redact_sensitive_error(error))
    }

    async fn put_object_legal_hold(
        &self,
        path: &RemotePath,
        status: LegalHoldStatus,
        options: &ObjectLockOptions,
    ) -> Result<()> {
        let sdk_status = match status {
            LegalHoldStatus::On => aws_sdk_s3::types::ObjectLockLegalHoldStatus::On,
            LegalHoldStatus::Off => aws_sdk_s3::types::ObjectLockLegalHoldStatus::Off,
        };
        let legal_hold = aws_sdk_s3::types::ObjectLockLegalHold::builder()
            .status(sdk_status)
            .build();
        let mut request = self
            .inner
            .put_object_legal_hold()
            .bucket(&path.bucket)
            .key(&path.key)
            .legal_hold(legal_hold);
        if let Some(version_id) = &options.version_id {
            request = request.version_id(version_id);
        }
        request.send().await.map_err(|error| {
            self.redact_object_lock_service_error(
                &error,
                Self::map_object_request_error(&error, path, options.version_id.as_deref()),
            )
        })?;
        Ok(())
    }

    async fn get_bucket_encryption(&self, bucket: &str) -> Result<Option<BucketEncryption>> {
        let response = match self
            .inner
            .get_bucket_encryption()
            .bucket(bucket)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                let error_text = Self::format_sdk_error(&error);
                let missing_config =
                    if let aws_sdk_s3::error::SdkError::ServiceError(service_err) = &error {
                        is_missing_bucket_encryption_response(
                            service_err.err().code(),
                            Some(service_err.raw().status().as_u16()),
                            &error_text,
                        )
                    } else {
                        is_missing_bucket_encryption_response(None, None, &error_text)
                    };
                if missing_config {
                    return Ok(None);
                }
                return Err(Error::General(format!(
                    "get_bucket_encryption: {error_text}"
                )));
            }
        };

        let rule = response
            .server_side_encryption_configuration()
            .and_then(|config| config.rules().first())
            .and_then(|rule| rule.apply_server_side_encryption_by_default())
            .ok_or_else(|| {
                Error::General("get_bucket_encryption: missing bucket encryption rule".to_string())
            })?;

        sdk_bucket_encryption_to_core(rule).map(Some)
    }

    async fn set_bucket_encryption(
        &self,
        bucket: &str,
        encryption: BucketEncryption,
    ) -> Result<()> {
        let configuration = core_bucket_encryption_to_sdk(&encryption);

        self.inner
            .put_bucket_encryption()
            .bucket(bucket)
            .server_side_encryption_configuration(configuration)
            .send()
            .await
            .map_err(|e| Error::General(format!("set_bucket_encryption: {e}")))?;

        Ok(())
    }

    async fn delete_bucket_encryption(&self, bucket: &str) -> Result<()> {
        match self
            .inner
            .delete_bucket_encryption()
            .bucket(bucket)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(error) => {
                let error_text = Self::format_sdk_error(&error);
                let missing_config =
                    if let aws_sdk_s3::error::SdkError::ServiceError(service_err) = &error {
                        is_missing_bucket_encryption_response(
                            service_err.err().code(),
                            Some(service_err.raw().status().as_u16()),
                            &error_text,
                        )
                    } else {
                        is_missing_bucket_encryption_response(None, None, &error_text)
                    };
                if missing_config {
                    Ok(())
                } else {
                    Err(Error::General(format!(
                        "delete_bucket_encryption: {error_text}"
                    )))
                }
            }
        }
    }

    async fn list_object_versions(
        &self,
        path: &RemotePath,
        max_keys: Option<i32>,
    ) -> Result<Vec<ObjectVersion>> {
        Ok(self.list_object_versions_page(path, max_keys).await?.items)
    }

    async fn list_object_versions_page_with_options(
        &self,
        path: &RemotePath,
        options: &ListObjectVersionsOptions,
    ) -> Result<ObjectVersionListResult> {
        self.list_object_versions_page_with_markers(
            path,
            options.max_keys,
            options.key_marker.as_deref(),
            options.version_id_marker.as_deref(),
        )
        .await
    }

    async fn get_object_tags(
        &self,
        path: &RemotePath,
    ) -> Result<std::collections::HashMap<String, String>> {
        let response = match self
            .inner
            .get_object_tagging()
            .bucket(&path.bucket)
            .key(&path.key)
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => {
                if e.to_string().contains("NoSuchTagSet") {
                    return Ok(std::collections::HashMap::new());
                }
                return Err(Error::General(format!("get_object_tags: {e}")));
            }
        };

        let mut tags = std::collections::HashMap::new();
        for tag in response.tag_set() {
            let key = tag.key();
            let value = tag.value();
            tags.insert(key.to_string(), value.to_string());
        }

        Ok(tags)
    }

    async fn get_bucket_tags(
        &self,
        bucket: &str,
    ) -> Result<std::collections::HashMap<String, String>> {
        let response = match self.inner.get_bucket_tagging().bucket(bucket).send().await {
            Ok(response) => response,
            Err(e) => {
                if e.to_string().contains("NoSuchTagSet") {
                    return Ok(std::collections::HashMap::new());
                }
                return Err(Error::General(format!("get_bucket_tags: {e}")));
            }
        };

        let mut tags = std::collections::HashMap::new();
        for tag in response.tag_set() {
            let key = tag.key();
            let value = tag.value();
            tags.insert(key.to_string(), value.to_string());
        }

        Ok(tags)
    }

    async fn set_object_tags(
        &self,
        path: &RemotePath,
        tags: std::collections::HashMap<String, String>,
    ) -> Result<()> {
        let tagging = build_tagging(tags)?;

        self.inner
            .put_object_tagging()
            .bucket(&path.bucket)
            .key(&path.key)
            .tagging(tagging)
            .send()
            .await
            .map_err(|e| Error::General(format!("set_object_tags: {e}")))?;

        Ok(())
    }

    async fn set_bucket_tags(
        &self,
        bucket: &str,
        tags: std::collections::HashMap<String, String>,
    ) -> Result<()> {
        let tagging = build_tagging(tags)?;

        self.inner
            .put_bucket_tagging()
            .bucket(bucket)
            .tagging(tagging)
            .send()
            .await
            .map_err(|e| Error::General(format!("set_bucket_tags: {e}")))?;

        Ok(())
    }

    async fn delete_object_tags(&self, path: &RemotePath) -> Result<()> {
        self.inner
            .delete_object_tagging()
            .bucket(&path.bucket)
            .key(&path.key)
            .send()
            .await
            .map_err(|e| Error::General(format!("delete_object_tags: {e}")))?;

        Ok(())
    }

    async fn delete_bucket_tags(&self, bucket: &str) -> Result<()> {
        self.inner
            .delete_bucket_tagging()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| Error::General(format!("delete_bucket_tags: {e}")))?;

        Ok(())
    }

    async fn get_bucket_policy(&self, bucket: &str) -> Result<Option<String>> {
        let response = match self.inner.get_bucket_policy().bucket(bucket).send().await {
            Ok(policy) => policy,
            Err(error) => {
                let error_text = error.to_string();
                let kind = if let aws_sdk_s3::error::SdkError::ServiceError(service_err) = &error {
                    let code = service_err
                        .raw()
                        .headers()
                        .get("x-amz-error-code")
                        .and_then(|value| std::str::from_utf8(value.as_bytes()).ok());
                    let status = Some(service_err.raw().status().as_u16());
                    Self::bucket_policy_error_kind(code, status, &error_text)
                } else {
                    Self::bucket_policy_error_kind(None, None, &error_text)
                };
                return Self::map_get_bucket_policy_error(bucket, kind, &error_text);
            }
        };

        Ok(response.policy().map(|policy| policy.to_string()))
    }

    async fn set_bucket_policy(&self, bucket: &str, policy: &str) -> Result<()> {
        self.inner
            .put_bucket_policy()
            .bucket(bucket)
            .policy(policy)
            .send()
            .await
            .map_err(|e| Error::General(format!("set_bucket_policy: {e}")))?;

        Ok(())
    }

    async fn delete_bucket_policy(&self, bucket: &str) -> Result<()> {
        match self
            .inner
            .delete_bucket_policy()
            .bucket(bucket)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                let error_text = e.to_string();
                let kind = if let aws_sdk_s3::error::SdkError::ServiceError(service_err) = &e {
                    let code = service_err
                        .raw()
                        .headers()
                        .get("x-amz-error-code")
                        .and_then(|value| std::str::from_utf8(value.as_bytes()).ok());
                    let status = Some(service_err.raw().status().as_u16());
                    Self::bucket_policy_error_kind(code, status, &error_text)
                } else {
                    Self::bucket_policy_error_kind(None, None, &error_text)
                };
                Self::map_delete_bucket_policy_error(bucket, kind, &error_text)
            }
        }
    }

    async fn get_bucket_cors(&self, bucket: &str) -> Result<Vec<CorsRule>> {
        let response = match self.inner.get_bucket_cors().bucket(bucket).send().await {
            Ok(response) => response,
            Err(error) => {
                let error_text = error.to_string();
                let missing_config =
                    if let aws_sdk_s3::error::SdkError::ServiceError(service_err) = &error {
                        is_missing_cors_configuration_response(
                            service_err.err().code(),
                            Some(service_err.raw().status().as_u16()),
                            &error_text,
                        )
                    } else {
                        is_missing_cors_configuration_response(None, None, &error_text)
                    };

                if missing_config {
                    return Ok(Vec::new());
                }

                if error_text.contains("service error")
                    && let Ok(url) = self.cors_url(bucket)
                {
                    match self.xml_request(Method::GET, url, None, None).await {
                        Ok(body) => return parse_cors_configuration_xml(&body),
                        Err(Error::Network(raw_error))
                            if is_missing_cors_configuration_error(&raw_error) =>
                        {
                            return Ok(Vec::new());
                        }
                        Err(_) => {}
                    }
                }

                return Err(Error::General(format!("get_bucket_cors: {error_text}")));
            }
        };

        Ok(response
            .cors_rules()
            .iter()
            .map(sdk_cors_rule_to_core)
            .collect())
    }

    async fn set_bucket_cors(&self, bucket: &str, rules: Vec<CorsRule>) -> Result<()> {
        let cors_rules = rules
            .iter()
            .map(core_cors_rule_to_sdk)
            .collect::<Result<Vec<_>>>()?;
        let cors_configuration = aws_sdk_s3::types::CorsConfiguration::builder()
            .set_cors_rules(Some(cors_rules))
            .build()
            .map_err(|e| Error::General(format!("build bucket cors config: {e}")))?;

        self.inner
            .put_bucket_cors()
            .bucket(bucket)
            .cors_configuration(cors_configuration)
            .send()
            .await
            .map_err(|e| Error::General(format!("set_bucket_cors: {e}")))?;

        Ok(())
    }

    async fn delete_bucket_cors(&self, bucket: &str) -> Result<()> {
        match self.inner.delete_bucket_cors().bucket(bucket).send().await {
            Ok(_) => Ok(()),
            Err(error) => {
                let error_text = error.to_string();
                let missing_config =
                    if let aws_sdk_s3::error::SdkError::ServiceError(service_err) = &error {
                        is_missing_cors_configuration_response(
                            service_err.err().code(),
                            Some(service_err.raw().status().as_u16()),
                            &error_text,
                        )
                    } else {
                        is_missing_cors_configuration_response(None, None, &error_text)
                    };

                if missing_config {
                    return Ok(());
                }

                if error_text.contains("service error")
                    && let Ok(url) = self.cors_url(bucket)
                {
                    match self.xml_request(Method::DELETE, url, None, None).await {
                        Ok(_) => return Ok(()),
                        Err(Error::Network(raw_error))
                            if is_missing_cors_configuration_error(&raw_error) =>
                        {
                            return Ok(());
                        }
                        Err(_) => {}
                    }
                }

                Err(Error::General(format!("delete_bucket_cors: {error_text}")))
            }
        }
    }

    async fn get_bucket_notifications(&self, bucket: &str) -> Result<Vec<BucketNotification>> {
        let response = self
            .inner
            .get_bucket_notification_configuration()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| Error::General(format!("get_bucket_notifications: {e}")))?;

        let mut rules = Vec::new();

        for cfg in response.queue_configurations() {
            let (prefix, suffix) = Self::extract_notification_filter(cfg.filter());
            rules.push(BucketNotification {
                id: cfg.id().map(ToString::to_string),
                target: NotificationTarget::Queue,
                arn: cfg.queue_arn().to_string(),
                events: Self::event_list_to_strings(cfg.events()),
                prefix,
                suffix,
            });
        }

        for cfg in response.topic_configurations() {
            let (prefix, suffix) = Self::extract_notification_filter(cfg.filter());
            rules.push(BucketNotification {
                id: cfg.id().map(ToString::to_string),
                target: NotificationTarget::Topic,
                arn: cfg.topic_arn().to_string(),
                events: Self::event_list_to_strings(cfg.events()),
                prefix,
                suffix,
            });
        }

        for cfg in response.lambda_function_configurations() {
            let (prefix, suffix) = Self::extract_notification_filter(cfg.filter());
            rules.push(BucketNotification {
                id: cfg.id().map(ToString::to_string),
                target: NotificationTarget::Lambda,
                arn: cfg.lambda_function_arn().to_string(),
                events: Self::event_list_to_strings(cfg.events()),
                prefix,
                suffix,
            });
        }

        Ok(rules)
    }

    async fn set_bucket_notifications(
        &self,
        bucket: &str,
        notifications: Vec<BucketNotification>,
    ) -> Result<()> {
        use aws_sdk_s3::types::{
            LambdaFunctionConfiguration, NotificationConfiguration, QueueConfiguration,
            TopicConfiguration,
        };

        let expected_notifications = notifications.clone();
        let mut queues = Vec::new();
        let mut topics = Vec::new();
        let mut lambdas = Vec::new();

        for rule in notifications {
            let events = Self::strings_to_event_list(&rule.events);
            if events.is_empty() {
                return Err(Error::General(format!(
                    "set_bucket_notifications: empty event list for target '{}'",
                    rule.arn
                )));
            }

            let filter =
                Self::build_notification_filter(rule.prefix.as_deref(), rule.suffix.as_deref());

            match rule.target {
                NotificationTarget::Queue => {
                    let mut builder = QueueConfiguration::builder()
                        .queue_arn(rule.arn)
                        .set_events(Some(events))
                        .set_id(rule.id);
                    if let Some(filter) = filter {
                        builder = builder.filter(filter);
                    }
                    let queue = builder
                        .build()
                        .map_err(|e| Error::General(format!("build queue notification: {e}")))?;
                    queues.push(queue);
                }
                NotificationTarget::Topic => {
                    let mut builder = TopicConfiguration::builder()
                        .topic_arn(rule.arn)
                        .set_events(Some(events))
                        .set_id(rule.id);
                    if let Some(filter) = filter {
                        builder = builder.filter(filter);
                    }
                    let topic = builder
                        .build()
                        .map_err(|e| Error::General(format!("build topic notification: {e}")))?;
                    topics.push(topic);
                }
                NotificationTarget::Lambda => {
                    let mut builder = LambdaFunctionConfiguration::builder()
                        .lambda_function_arn(rule.arn)
                        .set_events(Some(events))
                        .set_id(rule.id);
                    if let Some(filter) = filter {
                        builder = builder.filter(filter);
                    }
                    let lambda = builder
                        .build()
                        .map_err(|e| Error::General(format!("build lambda notification: {e}")))?;
                    lambdas.push(lambda);
                }
            }
        }

        let config = NotificationConfiguration::builder()
            .set_queue_configurations(Some(queues))
            .set_topic_configurations(Some(topics))
            .set_lambda_function_configurations(Some(lambdas))
            .build();

        match self
            .inner
            .put_bucket_notification_configuration()
            .bucket(bucket)
            .notification_configuration(config)
            .send()
            .await
        {
            Ok(_) => {}
            Err(error) => {
                let error_text = error.to_string();
                // RustFS may apply notification configuration but still return a non-AWS
                // response envelope that the SDK reports as "service error".
                if error_text.contains("service error")
                    && let Ok(actual) = self.get_bucket_notifications(bucket).await
                    && Self::notifications_equivalent(&expected_notifications, &actual)
                {
                    return Ok(());
                }
                return Err(Error::General(format!(
                    "set_bucket_notifications: {error_text}"
                )));
            }
        }

        Ok(())
    }

    async fn get_bucket_lifecycle(&self, bucket: &str) -> Result<Vec<LifecycleRule>> {
        let response = match self
            .inner
            .get_bucket_lifecycle_configuration()
            .bucket(bucket)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(error) => {
                let error_text = Self::format_sdk_error(&error);
                if error_text.contains("NoSuchLifecycleConfiguration")
                    || error_text.contains("lifecycle configuration is not found")
                {
                    return Ok(Vec::new());
                }
                return Err(Error::General(format!(
                    "get_bucket_lifecycle: {error_text}"
                )));
            }
        };

        let mut rules = Vec::new();
        for sdk_rule in response.rules() {
            let id = sdk_rule.id().unwrap_or("").to_string();
            let status = match sdk_rule.status().as_str() {
                "Enabled" => rc_core::LifecycleRuleStatus::Enabled,
                _ => rc_core::LifecycleRuleStatus::Disabled,
            };

            let prefix = parse_lifecycle_filter_prefix(sdk_rule.filter());
            let tags = parse_lifecycle_filter_tags(sdk_rule.filter());

            let expiration = sdk_rule
                .expiration()
                .map(|exp| rc_core::LifecycleExpiration {
                    days: exp.days(),
                    date: exp.date().map(|d| d.to_string()),
                });

            let transition = sdk_rule
                .transitions()
                .first()
                .map(|t| rc_core::LifecycleTransition {
                    days: t.days(),
                    date: t.date().map(|d| d.to_string()),
                    storage_class: t
                        .storage_class()
                        .map(|sc| sc.as_str().to_string())
                        .unwrap_or_default(),
                });

            let noncurrent_version_expiration =
                sdk_rule.noncurrent_version_expiration().map(|nve| {
                    rc_core::NoncurrentVersionExpiration {
                        noncurrent_days: nve.noncurrent_days().unwrap_or(0),
                        newer_noncurrent_versions: nve.newer_noncurrent_versions(),
                    }
                });

            let noncurrent_version_transition = sdk_rule
                .noncurrent_version_transitions()
                .first()
                .map(|nvt| rc_core::NoncurrentVersionTransition {
                    noncurrent_days: nvt.noncurrent_days().unwrap_or(0),
                    storage_class: nvt
                        .storage_class()
                        .map(|sc| sc.as_str().to_string())
                        .unwrap_or_default(),
                });

            let abort_incomplete_multipart_upload_days = sdk_rule
                .abort_incomplete_multipart_upload()
                .and_then(|a| a.days_after_initiation());

            let expired_object_delete_marker = sdk_rule
                .expiration()
                .and_then(|e| e.expired_object_delete_marker())
                .filter(|v| *v);

            rules.push(LifecycleRule {
                id,
                status,
                prefix,
                tags,
                expiration,
                transition,
                noncurrent_version_expiration,
                noncurrent_version_transition,
                abort_incomplete_multipart_upload_days,
                expired_object_delete_marker,
            });
        }

        Ok(rules)
    }

    async fn set_bucket_lifecycle(&self, bucket: &str, rules: Vec<LifecycleRule>) -> Result<()> {
        use aws_sdk_s3::types::{
            AbortIncompleteMultipartUpload, BucketLifecycleConfiguration, ExpirationStatus,
            LifecycleExpiration as SdkExpiration, LifecycleRule as SdkRule,
            NoncurrentVersionExpiration as SdkNve, NoncurrentVersionTransition as SdkNvt,
            Transition, TransitionStorageClass,
        };

        let mut sdk_rules = Vec::new();
        for rule in rules {
            validate_lifecycle_rule(&rule)?;
            let status = match rule.status {
                rc_core::LifecycleRuleStatus::Enabled => ExpirationStatus::Enabled,
                rc_core::LifecycleRuleStatus::Disabled => ExpirationStatus::Disabled,
            };

            let filter = build_lifecycle_rule_filter(rule.prefix.as_deref(), rule.tags.as_ref())?;

            let expiration =
                if rule.expiration.is_some() || rule.expired_object_delete_marker == Some(true) {
                    let mut builder = SdkExpiration::builder();
                    if let Some(days) = rule
                        .expiration
                        .as_ref()
                        .and_then(|expiration| expiration.days)
                    {
                        builder = builder.days(days);
                    }
                    if let Some(date_str) = rule
                        .expiration
                        .as_ref()
                        .and_then(|expiration| expiration.date.as_deref())
                        && let Ok(dt) = aws_smithy_types::DateTime::from_str(
                            date_str,
                            aws_smithy_types::date_time::Format::DateTime,
                        )
                    {
                        builder = builder.date(dt);
                    }
                    if let Some(true) = rule.expired_object_delete_marker {
                        builder = builder.expired_object_delete_marker(true);
                    }
                    Some(builder.build())
                } else {
                    None
                };

            let transitions = rule.transition.map(|t| {
                #[allow(deprecated)]
                let sc = TransitionStorageClass::from(t.storage_class.as_str());
                let mut builder = Transition::builder().storage_class(sc);
                if let Some(days) = t.days {
                    builder = builder.days(days);
                }
                if let Some(ref date_str) = t.date
                    && let Ok(dt) = aws_smithy_types::DateTime::from_str(
                        date_str,
                        aws_smithy_types::date_time::Format::DateTime,
                    )
                {
                    builder = builder.date(dt);
                }
                vec![builder.build()]
            });

            let nve = rule.noncurrent_version_expiration.map(|nve| {
                let mut builder = SdkNve::builder().noncurrent_days(nve.noncurrent_days);
                if let Some(newer) = nve.newer_noncurrent_versions {
                    builder = builder.newer_noncurrent_versions(newer);
                }
                builder.build()
            });

            let nvt = rule.noncurrent_version_transition.map(|nvt| {
                let sc = TransitionStorageClass::from(nvt.storage_class.as_str());
                let builder = SdkNvt::builder()
                    .noncurrent_days(nvt.noncurrent_days)
                    .storage_class(sc);
                vec![builder.build()]
            });

            let abort = rule.abort_incomplete_multipart_upload_days.map(|days| {
                AbortIncompleteMultipartUpload::builder()
                    .days_after_initiation(days)
                    .build()
            });

            let mut builder = SdkRule::builder().id(&rule.id).status(status);
            if let Some(filter) = filter {
                builder = builder.filter(filter);
            }
            if let Some(expiration) = expiration {
                builder = builder.expiration(expiration);
            }
            if let Some(transitions) = transitions {
                builder = builder.set_transitions(Some(transitions));
            }
            if let Some(nve) = nve {
                builder = builder.noncurrent_version_expiration(nve);
            }
            if let Some(nvt) = nvt {
                builder = builder.set_noncurrent_version_transitions(Some(nvt));
            }
            if let Some(abort) = abort {
                builder = builder.abort_incomplete_multipart_upload(abort);
            }

            let sdk_rule = builder
                .build()
                .map_err(|e| Error::General(format!("build lifecycle rule: {e}")))?;
            sdk_rules.push(sdk_rule);
        }

        let config = BucketLifecycleConfiguration::builder()
            .set_rules(Some(sdk_rules))
            .build()
            .map_err(|e| Error::General(format!("build lifecycle config: {e}")))?;

        self.inner
            .put_bucket_lifecycle_configuration()
            .bucket(bucket)
            .lifecycle_configuration(config)
            .send()
            .await
            .map_err(|e| {
                Error::General(format!(
                    "set_bucket_lifecycle: {}",
                    Self::format_sdk_error(&e)
                ))
            })?;

        Ok(())
    }

    async fn delete_bucket_lifecycle(&self, bucket: &str) -> Result<()> {
        self.inner
            .delete_bucket_lifecycle()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| {
                Error::General(format!(
                    "delete_bucket_lifecycle: {}",
                    Self::format_sdk_error(&e)
                ))
            })?;
        Ok(())
    }

    async fn restore_object(&self, path: &RemotePath, days: i32) -> Result<()> {
        use aws_sdk_s3::types::RestoreRequest;

        let request = RestoreRequest::builder().days(days).build();
        self.inner
            .restore_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .restore_request(request)
            .send()
            .await
            .map_err(|e| {
                Error::General(format!("restore_object: {}", Self::format_sdk_error(&e)))
            })?;
        Ok(())
    }

    async fn get_bucket_replication(
        &self,
        bucket: &str,
    ) -> Result<Option<ReplicationConfiguration>> {
        let url = self.replication_url(bucket)?;
        let body = match self.xml_request(Method::GET, url, None, None).await {
            Ok(body) => body,
            Err(Error::Network(error_text))
                if error_text.contains("ReplicationConfigurationNotFound")
                    || error_text.contains("replication configuration is not found")
                    || error_text.contains("replication not found") =>
            {
                return Ok(None);
            }
            Err(error) => {
                return Err(Error::General(format!("get_bucket_replication: {error}")));
            }
        };

        parse_replication_configuration_xml(&body).map(Some)
    }

    async fn set_bucket_replication(
        &self,
        bucket: &str,
        config: ReplicationConfiguration,
    ) -> Result<()> {
        let url = self.replication_url(bucket)?;
        let body = build_replication_configuration_xml(&config).into_bytes();
        self.xml_request(Method::PUT, url, Some("application/xml"), Some(body))
            .await
            .map_err(|e| Error::General(format!("set_bucket_replication: {e}")))?;

        Ok(())
    }

    async fn delete_bucket_replication(&self, bucket: &str) -> Result<()> {
        self.inner
            .delete_bucket_replication()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| {
                Error::General(format!(
                    "delete_bucket_replication: {}",
                    Self::format_sdk_error(&e)
                ))
            })?;
        Ok(())
    }

    async fn check_bucket_replication(&self, bucket: &str) -> Result<()> {
        let result = self.check_bucket_replication_detailed(bucket).await?;
        if result.succeeded() {
            Ok(())
        } else {
            Err(Error::Conflict(
                "One or more replication targets failed the active check".to_string(),
            ))
        }
    }

    async fn check_bucket_replication_detailed(
        &self,
        bucket: &str,
    ) -> Result<ReplicationCheckResult> {
        let url = self.replication_extension_url(bucket, "replication-check", &[])?;
        let body = self
            .signed_replication_extension_request(Method::GET, url)
            .await?;
        self.parse_replication_check_response(&body)
    }

    async fn start_bucket_replication_resync(
        &self,
        bucket: &str,
        options: ReplicationResyncStartOptions,
    ) -> Result<ReplicationResyncStartResult> {
        let mut query = Vec::new();
        if let Some(target_arn) = options.target_arn {
            query.push(("arn", target_arn));
        }
        if let Some(older_than) = options.older_than {
            query.push((
                "older-than",
                humantime::format_duration(older_than).to_string(),
            ));
        }
        if let Some(reset_id) = options.reset_id {
            query.push(("reset-id", reset_id));
        }
        let url = self.replication_extension_url(bucket, "replication-reset", &query)?;
        let body = self
            .signed_replication_extension_request(Method::PUT, url)
            .await?;
        let mut response: ReplicationResyncResponseDto =
            serde_json::from_slice(&body).map_err(|error| {
                Error::General(format!("Malformed replication start response: {error}"))
            })?;
        if response.targets.len() != 1 {
            return Err(Error::General(format!(
                "Malformed replication start response: expected one target, got {}",
                response.targets.len()
            )));
        }
        let target = response
            .targets
            .pop()
            .expect("one target was verified before removing it");
        if target.arn.is_empty() || target.reset_id.is_empty() {
            return Err(Error::General(
                "Malformed replication start response: missing ARN or reset ID".to_string(),
            ));
        }
        Ok(ReplicationResyncStartResult {
            target_arn: target.arn,
            reset_id: target.reset_id,
        })
    }

    async fn bucket_replication_resync_status(
        &self,
        bucket: &str,
        target_arn: Option<&str>,
    ) -> Result<ReplicationResyncStatus> {
        let query = target_arn
            .map(|target_arn| vec![("arn", target_arn.to_string())])
            .unwrap_or_default();
        let url = self.replication_extension_url(bucket, "replication-reset-status", &query)?;
        let body = self
            .signed_replication_extension_request(Method::GET, url)
            .await?;
        let response: ReplicationResyncResponseDto =
            serde_json::from_slice(&body).map_err(|error| {
                Error::General(format!("Malformed replication status response: {error}"))
            })?;
        let targets = response
            .targets
            .into_iter()
            .map(Self::convert_resync_status_target)
            .collect::<Result<Vec<_>>>()?;
        Ok(ReplicationResyncStatus { targets })
    }

    async fn select_object_content(
        &self,
        path: &RemotePath,
        options: &SelectOptions,
        writer: &mut (dyn AsyncWrite + Send + Unpin),
    ) -> Result<()> {
        crate::select::select_object_content(&self.inner, path, options, writer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_smithy_http_client::test_util::{
        CaptureRequestReceiver, ReplayEvent, StaticReplayClient, capture_request,
    };
    use rc_core::{
        ChecksumAlgorithm, ChecksumRequest, MetadataDirective, ObjectAttributes,
        ObjectWriteEncryption, ObjectWriteOptions, S3_MULTIPART_COPY_MIN_PART_SIZE, SseCustomerKey,
        TransferCopyOptions, TransferReadOptions,
    };
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};
    use tokio::sync::Notify;

    #[derive(Debug)]
    struct CapturedXmlRequest {
        method: String,
        target: String,
        headers: Vec<(String, String)>,
    }

    fn test_s3_client(
        response: Option<http::Response<SdkBody>>,
    ) -> (S3Client, CaptureRequestReceiver) {
        test_s3_client_with_endpoint("https://example.com", response)
    }

    fn test_s3_client_with_endpoint(
        endpoint: &str,
        response: Option<http::Response<SdkBody>>,
    ) -> (S3Client, CaptureRequestReceiver) {
        test_s3_client_with_endpoint_and_headers(endpoint, response, Vec::new())
    }

    fn test_s3_client_with_endpoint_and_headers(
        endpoint: &str,
        response: Option<http::Response<SdkBody>>,
        request_headers: Vec<RequestHeader>,
    ) -> (S3Client, CaptureRequestReceiver) {
        let (http_client, request_receiver) = capture_request(response);
        let credentials = Credentials::new(
            "access-key",
            "secret-key",
            None,
            None,
            "rc-test-credentials",
        );
        let mut config_builder = aws_sdk_s3::config::Builder::new()
            .credentials_provider(credentials)
            .endpoint_url(endpoint)
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .force_path_style(true)
            .retry_config(aws_smithy_types::retry::RetryConfig::disabled())
            .behavior_version_latest()
            .http_client(http_client);

        let presign_config = config_builder.clone().build();

        if !request_headers.is_empty() {
            config_builder = config_builder.interceptor(CustomHeaderInterceptor {
                headers: request_headers.clone(),
            });
        }

        let config = config_builder.build();

        let alias = Alias::new("test", endpoint, "access-key", "secret-key");
        let xml_http_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("build redirect-disabled XML test client");
        let client = S3Client {
            inner: aws_sdk_s3::Client::from_conf(config),
            presign_inner: aws_sdk_s3::Client::from_conf(presign_config),
            xml_http_client,
            alias,
            request_headers,
        };

        (client, request_receiver)
    }

    fn test_s3_client_with_response_sequence(
        responses: Vec<http::Response<SdkBody>>,
    ) -> (S3Client, StaticReplayClient) {
        test_s3_client_with_response_sequence_and_headers(responses, Vec::new())
    }

    fn test_s3_client_with_response_sequence_and_headers(
        responses: Vec<http::Response<SdkBody>>,
        request_headers: Vec<RequestHeader>,
    ) -> (S3Client, StaticReplayClient) {
        let events = responses
            .into_iter()
            .enumerate()
            .map(|(index, response)| {
                let request = http::Request::builder()
                    .uri(format!("https://example.com/expected-{index}"))
                    .body(SdkBody::empty())
                    .expect("build replay request");
                ReplayEvent::new(request, response)
            })
            .collect();
        let replay = StaticReplayClient::new(events);
        let credentials = Credentials::new(
            "access-key",
            "secret-key",
            None,
            None,
            "rc-test-credentials",
        );
        let mut config_builder = aws_sdk_s3::config::Builder::new()
            .credentials_provider(credentials)
            .endpoint_url("https://example.com")
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .force_path_style(true)
            .retry_config(aws_smithy_types::retry::RetryConfig::disabled())
            .behavior_version_latest()
            .http_client(replay.clone());
        let presign_config = config_builder.clone().build();
        if !request_headers.is_empty() {
            config_builder = config_builder.interceptor(CustomHeaderInterceptor {
                headers: request_headers.clone(),
            });
        }
        let config = config_builder.build();
        let alias = Alias::new("test", "https://example.com", "access-key", "secret-key");
        let client = S3Client {
            inner: aws_sdk_s3::Client::from_conf(config),
            presign_inner: aws_sdk_s3::Client::from_conf(presign_config),
            xml_http_client: reqwest::Client::new(),
            alias,
            request_headers,
        };
        (client, replay)
    }

    #[derive(Debug)]
    enum BlockingReplayEvent {
        Response(Box<aws_smithy_runtime_api::client::orchestrator::HttpResponse>),
        Pending,
    }

    #[derive(Debug, Clone)]
    struct BlockingReplayClient {
        events: Arc<Mutex<VecDeque<BlockingReplayEvent>>>,
        requests: Arc<Mutex<Vec<(String, String)>>>,
        pending_started: Arc<AtomicBool>,
        pending_notify: Arc<Notify>,
    }

    impl BlockingReplayClient {
        fn new(
            create_response: http::Response<SdkBody>,
            abort_response: http::Response<SdkBody>,
        ) -> Self {
            let events = VecDeque::from([
                BlockingReplayEvent::Response(Box::new(
                    create_response.try_into().expect("valid create response"),
                )),
                BlockingReplayEvent::Pending,
                BlockingReplayEvent::Response(Box::new(
                    abort_response.try_into().expect("valid abort response"),
                )),
            ]);
            Self {
                events: Arc::new(Mutex::new(events)),
                requests: Arc::new(Mutex::new(Vec::new())),
                pending_started: Arc::new(AtomicBool::new(false)),
                pending_notify: Arc::new(Notify::new()),
            }
        }

        async fn wait_for_pending_request(&self) {
            let notified = self.pending_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.pending_started.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }

        fn requests(&self) -> Vec<(String, String)> {
            self.requests.lock().expect("request lock").clone()
        }
    }

    impl HttpConnector for BlockingReplayClient {
        fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
            self.requests
                .lock()
                .expect("request lock")
                .push((request.method().to_string(), request.uri().to_string()));
            let event = self.events.lock().expect("event lock").pop_front();
            match event {
                Some(BlockingReplayEvent::Response(response)) => {
                    HttpConnectorFuture::ready(Ok(*response))
                }
                Some(BlockingReplayEvent::Pending) => {
                    self.pending_started.store(true, Ordering::Release);
                    self.pending_notify.notify_waiters();
                    HttpConnectorFuture::new(async {
                        futures::future::pending::<
                            std::result::Result<
                                aws_smithy_runtime_api::client::orchestrator::HttpResponse,
                                ConnectorError,
                            >,
                        >()
                        .await
                    })
                }
                None => HttpConnectorFuture::ready(Err(ConnectorError::other(
                    "BlockingReplayClient has no remaining response".into(),
                    None,
                ))),
            }
        }
    }

    impl HttpClient for BlockingReplayClient {
        fn http_connector(
            &self,
            _settings: &HttpConnectorSettings,
            _runtime_components: &RuntimeComponents,
        ) -> SharedHttpConnector {
            SharedHttpConnector::new(self.clone())
        }
    }

    fn test_s3_client_with_pending_part() -> (S3Client, BlockingReplayClient) {
        let abort_response = http::Response::builder()
            .status(204)
            .body(SdkBody::empty())
            .expect("build abort response");
        let replay = BlockingReplayClient::new(
            multipart_copy_create_response("cancel-upload-id"),
            abort_response,
        );
        let credentials = Credentials::new(
            "access-key",
            "secret-key",
            None,
            None,
            "rc-test-credentials",
        );
        let config = aws_sdk_s3::config::Builder::new()
            .credentials_provider(credentials)
            .endpoint_url("https://example.com")
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .force_path_style(true)
            .retry_config(aws_smithy_types::retry::RetryConfig::disabled())
            .behavior_version_latest()
            .http_client(replay.clone())
            .build();
        let alias = Alias::new("test", "https://example.com", "access-key", "secret-key");
        let client = S3Client {
            inner: aws_sdk_s3::Client::from_conf(config.clone()),
            presign_inner: aws_sdk_s3::Client::from_conf(config),
            xml_http_client: reqwest::Client::new(),
            alias,
            request_headers: Vec::new(),
        };
        (client, replay)
    }

    #[tokio::test]
    async fn head_object_maps_bare_http_404_to_not_found() {
        let response = http::Response::builder()
            .status(404)
            .body(SdkBody::empty())
            .expect("build head object response");
        let (client, _) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "missing.txt");

        let result = client.head_object(&path).await;

        assert!(
            matches!(result, Err(Error::NotFound(_))),
            "unexpected result: {result:?}"
        );
    }

    fn read_xml_request(stream: &mut TcpStream) -> CapturedXmlRequest {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut chunk).expect("read HTTP request");
            assert!(read > 0, "client closed connection before headers");
            buffer.extend_from_slice(&chunk[..read]);

            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };

        let headers_text = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
        let content_length = headers_text
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("valid content length"))
            })
            .unwrap_or(0);

        while buffer.len() - header_end < content_length {
            let read = stream.read(&mut chunk).expect("read HTTP request body");
            assert!(read > 0, "client closed connection before body");
            buffer.extend_from_slice(&chunk[..read]);
        }

        let mut lines = headers_text.lines();
        let request_line = lines.next().expect("request line");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().expect("request method").to_string();
        let target = parts.next().expect("request target").to_string();
        let headers = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.to_ascii_lowercase(), value.trim().to_string()))
            })
            .collect();

        CapturedXmlRequest {
            method,
            target,
            headers,
        }
    }

    fn start_xml_test_server() -> (
        String,
        mpsc::Receiver<CapturedXmlRequest>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener
            .set_nonblocking(true)
            .expect("configure nonblocking listener");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let (sender, receiver) = mpsc::channel();

        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "timed out waiting for request");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept request: {error}"),
                }
            };
            stream
                .set_nonblocking(false)
                .expect("configure blocking request stream");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set stream read timeout");
            let request = read_xml_request(&mut stream);
            sender.send(request).expect("send captured request");

            let response = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";
            stream
                .write_all(response.as_bytes())
                .expect("write HTTP response");
        });

        (endpoint, receiver, handle)
    }

    fn start_replication_extension_test_server(
        response: Vec<u8>,
    ) -> (
        String,
        mpsc::Receiver<CapturedXmlRequest>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let (sender, receiver) = mpsc::channel();

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set request timeout");
            let request = read_xml_request(&mut stream);
            sender.send(request).expect("send captured request");
            stream.write_all(&response).expect("write response");
        });

        (endpoint, receiver, handle)
    }

    fn start_repeated_replication_extension_test_server(
        response: Vec<u8>,
        request_count: usize,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));

        let handle = thread::spawn(move || {
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().expect("accept request");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("set request timeout");
                let _ = read_xml_request(&mut stream);
                stream.write_all(&response).expect("write response");
            }
        });

        (endpoint, handle)
    }

    fn start_counting_replication_extension_test_server(
        first_response: Vec<u8>,
    ) -> (String, mpsc::Receiver<usize>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener
            .set_nonblocking(true)
            .expect("configure nonblocking listener");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        let (sender, receiver) = mpsc::channel();

        let handle = thread::spawn(move || {
            let first_deadline = Instant::now() + Duration::from_secs(5);
            let mut first_stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < first_deadline,
                            "timed out waiting for request"
                        );
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept request: {error}"),
                }
            };
            first_stream
                .set_nonblocking(false)
                .expect("configure blocking request stream");
            let _ = read_xml_request(&mut first_stream);
            first_stream
                .write_all(&first_response)
                .expect("write first response");
            drop(first_stream);

            let mut request_count = 1;
            let follow_up_deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < follow_up_deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        request_count += 1;
                        stream
                            .set_nonblocking(false)
                            .expect("configure blocking follow-up stream");
                        let _ = read_xml_request(&mut stream);
                        let success_body =
                            br#"{"Targets":[{"Arn":"arn:target","ResetID":"server-id"}]}"#;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            success_body.len()
                        );
                        stream
                            .write_all(response.as_bytes())
                            .expect("write follow-up response headers");
                        stream
                            .write_all(success_body)
                            .expect("write follow-up response body");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept follow-up request: {error}"),
                }
            }
            sender.send(request_count).expect("send request count");
        });

        (endpoint, receiver, handle)
    }

    fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn test_object_info_creation() {
        let info = ObjectInfo::file("test.txt", 1024);
        assert_eq!(info.key, "test.txt");
        assert_eq!(info.size_bytes, Some(1024));
    }

    #[test]
    fn auto_bucket_lookup_uses_dns_for_aliyun_oss_service_endpoint() {
        let alias = Alias::new(
            "aliyun",
            "https://oss-cn-hangzhou.aliyuncs.com",
            "access-key",
            "secret-key",
        );

        assert!(!force_path_style_for_alias(&alias));
    }

    #[test]
    fn auto_bucket_lookup_uses_dns_for_aliyun_internal_service_endpoint() {
        let alias = Alias::new(
            "aliyun",
            "https://oss-cn-hangzhou-internal.aliyuncs.com",
            "access-key",
            "secret-key",
        );

        assert!(!force_path_style_for_alias(&alias));
    }

    #[test]
    fn auto_bucket_lookup_keeps_path_style_for_custom_endpoint() {
        let alias = Alias::new("local", "http://localhost:9000", "access-key", "secret-key");

        assert!(force_path_style_for_alias(&alias));
    }

    #[test]
    fn auto_bucket_lookup_keeps_path_style_for_non_oss_aliyun_endpoint() {
        let alias = Alias::new(
            "aliyun",
            "https://ecs-cn-hangzhou.aliyuncs.com",
            "access-key",
            "secret-key",
        );

        assert!(force_path_style_for_alias(&alias));
    }

    #[test]
    fn auto_bucket_lookup_keeps_path_style_for_invalid_endpoint() {
        let alias = Alias::new("broken", "not a valid endpoint", "access-key", "secret-key");

        assert!(force_path_style_for_alias(&alias));
    }

    #[test]
    fn explicit_bucket_lookup_overrides_auto_detection() {
        let mut path_alias = Alias::new(
            "aliyun",
            "https://oss-cn-hangzhou.aliyuncs.com",
            "access-key",
            "secret-key",
        );
        path_alias.bucket_lookup = "path".to_string();

        let mut dns_alias =
            Alias::new("local", "http://localhost:9000", "access-key", "secret-key");
        dns_alias.bucket_lookup = "dns".to_string();

        assert!(force_path_style_for_alias(&path_alias));
        assert!(!force_path_style_for_alias(&dns_alias));
    }

    #[test]
    fn unknown_bucket_lookup_keeps_path_style() {
        let mut alias = Alias::new(
            "aliyun",
            "https://oss-cn-hangzhou.aliyuncs.com",
            "access-key",
            "secret-key",
        );
        alias.bucket_lookup = "unexpected".to_string();

        assert!(force_path_style_for_alias(&alias));
    }

    #[test]
    fn parse_replication_configuration_xml_reads_delete_replication() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<ReplicationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Role>arn:rustfs:replication:us-east-1:123:test</Role>
  <Rule>
    <Status>Enabled</Status>
    <Destination>
      <Bucket>arn:rustfs:replication:us-east-1:123:dest</Bucket>
      <StorageClass>STANDARD</StorageClass>
    </Destination>
    <ID>rule-1</ID>
    <Priority>1</Priority>
    <Filter>
      <Prefix>logs/</Prefix>
    </Filter>
    <ExistingObjectReplication>
      <Status>Enabled</Status>
    </ExistingObjectReplication>
    <DeleteMarkerReplication>
      <Status>Disabled</Status>
    </DeleteMarkerReplication>
    <DeleteReplication>
      <Status>Enabled</Status>
    </DeleteReplication>
  </Rule>
</ReplicationConfiguration>"#;

        let config = parse_replication_configuration_xml(body).expect("parse replication xml");
        assert_eq!(config.role, "arn:rustfs:replication:us-east-1:123:test");
        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].id, "rule-1");
        assert_eq!(config.rules[0].prefix.as_deref(), Some("logs/"));
        assert_eq!(config.rules[0].delete_replication, Some(true));
        assert_eq!(config.rules[0].delete_marker_replication, Some(false));
        assert_eq!(config.rules[0].existing_object_replication, Some(true));
    }

    #[test]
    fn parse_replication_configuration_xml_preserves_tag_filters() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<ReplicationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Rule>
    <Status>Enabled</Status>
    <Destination>
      <Bucket>arn:rustfs:replication:us-east-1:123:dest</Bucket>
    </Destination>
    <ID>tagged-rule</ID>
    <Priority>2</Priority>
    <Filter>
      <And>
        <Prefix>logs/</Prefix>
        <Tag>
          <Key>env</Key>
          <Value>prod</Value>
        </Tag>
        <Tag>
          <Key>team</Key>
          <Value>core</Value>
        </Tag>
      </And>
    </Filter>
  </Rule>
</ReplicationConfiguration>"#;

        let config = parse_replication_configuration_xml(body).expect("parse replication xml");
        let rule = &config.rules[0];
        assert_eq!(rule.prefix.as_deref(), Some("logs/"));
        let tags = rule.tags.as_ref().expect("tag filters");
        assert_eq!(tags.get("env").map(String::as_str), Some("prod"));
        assert_eq!(tags.get("team").map(String::as_str), Some("core"));
    }

    #[test]
    fn build_replication_configuration_xml_writes_delete_replication() {
        let config = ReplicationConfiguration {
            role: "arn:rustfs:replication:us-east-1:123:test".to_string(),
            rules: vec![rc_core::ReplicationRule {
                id: "rule-1".to_string(),
                priority: 1,
                status: rc_core::ReplicationRuleStatus::Enabled,
                prefix: Some("logs/".to_string()),
                tags: None,
                destination: rc_core::ReplicationDestination {
                    bucket_arn: "arn:rustfs:replication:us-east-1:123:dest".to_string(),
                    storage_class: Some("STANDARD".to_string()),
                },
                delete_marker_replication: Some(true),
                existing_object_replication: Some(true),
                delete_replication: Some(true),
            }],
        };

        let xml = build_replication_configuration_xml(&config);
        assert!(xml.contains("<DeleteReplication><Status>Enabled</Status></DeleteReplication>"));
        assert!(xml.contains(
            "<ExistingObjectReplication><Status>Enabled</Status></ExistingObjectReplication>"
        ));
        assert!(xml.contains(
            "<DeleteMarkerReplication><Status>Enabled</Status></DeleteMarkerReplication>"
        ));
        assert!(xml.contains("<Filter><Prefix>logs/</Prefix></Filter>"));
    }

    #[test]
    fn build_replication_configuration_xml_writes_and_tag_filters() {
        let mut tags = HashMap::new();
        tags.insert("env".to_string(), "prod".to_string());
        tags.insert("team".to_string(), "core".to_string());

        let config = ReplicationConfiguration {
            role: String::new(),
            rules: vec![rc_core::ReplicationRule {
                id: "rule-1".to_string(),
                priority: 1,
                status: rc_core::ReplicationRuleStatus::Enabled,
                prefix: Some("logs/".to_string()),
                tags: Some(tags),
                destination: rc_core::ReplicationDestination {
                    bucket_arn: "arn:rustfs:replication:us-east-1:123:dest".to_string(),
                    storage_class: None,
                },
                delete_marker_replication: None,
                existing_object_replication: None,
                delete_replication: None,
            }],
        };

        let xml = build_replication_configuration_xml(&config);
        assert!(xml.contains("<Filter><And><Prefix>logs/</Prefix>"));
        assert!(xml.contains("<Tag><Key>env</Key><Value>prod</Value></Tag>"));
        assert!(xml.contains("<Tag><Key>team</Key><Value>core</Value></Tag>"));
    }

    #[test]
    fn build_lifecycle_rule_filter_preserves_prefix_and_tags() {
        let mut tags = HashMap::new();
        tags.insert("env".to_string(), "prod".to_string());
        tags.insert("team".to_string(), "core".to_string());

        let filter = build_lifecycle_rule_filter(Some("logs/"), Some(&tags))
            .expect("build lifecycle filter")
            .expect("lifecycle filter");

        assert_eq!(
            parse_lifecycle_filter_prefix(Some(&filter)).as_deref(),
            Some("logs/")
        );
        let parsed_tags = parse_lifecycle_filter_tags(Some(&filter)).expect("parsed tags");
        assert_eq!(parsed_tags.get("env").map(String::as_str), Some("prod"));
        assert_eq!(parsed_tags.get("team").map(String::as_str), Some("core"));
    }

    #[tokio::test]
    async fn set_bucket_lifecycle_serializes_marker_only_expiration() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::empty())
            .expect("build lifecycle response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let rule = LifecycleRule {
            id: "marker-only".to_string(),
            status: rc_core::LifecycleRuleStatus::Enabled,
            prefix: Some(String::new()),
            tags: None,
            expiration: None,
            transition: None,
            noncurrent_version_expiration: None,
            noncurrent_version_transition: None,
            abort_incomplete_multipart_upload_days: None,
            expired_object_delete_marker: Some(true),
        };

        ObjectStore::set_bucket_lifecycle(&client, "bucket", vec![rule])
            .await
            .expect("set marker-only lifecycle rule");

        let request = request_receiver.expect_request();
        let body = request.body().bytes().expect("request body bytes");
        let body = std::str::from_utf8(body).expect("request body is utf8");
        assert!(body.contains(
            "<Expiration><ExpiredObjectDeleteMarker>true</ExpiredObjectDeleteMarker></Expiration>"
        ));
    }

    #[tokio::test]
    async fn set_bucket_lifecycle_serializes_noncurrent_expiration_with_marker_cleanup() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::empty())
            .expect("build lifecycle response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let rule = LifecycleRule {
            id: "noncurrent-marker".to_string(),
            status: rc_core::LifecycleRuleStatus::Enabled,
            prefix: Some(String::new()),
            tags: None,
            expiration: None,
            transition: None,
            noncurrent_version_expiration: Some(rc_core::NoncurrentVersionExpiration {
                noncurrent_days: 1,
                newer_noncurrent_versions: None,
            }),
            noncurrent_version_transition: None,
            abort_incomplete_multipart_upload_days: None,
            expired_object_delete_marker: Some(true),
        };

        ObjectStore::set_bucket_lifecycle(&client, "bucket", vec![rule])
            .await
            .expect("set noncurrent lifecycle rule with marker cleanup");

        let request = request_receiver.expect_request();
        let body = request.body().bytes().expect("request body bytes");
        let body = std::str::from_utf8(body).expect("request body is utf8");
        assert!(body.contains(
            "<NoncurrentVersionExpiration><NoncurrentDays>1</NoncurrentDays></NoncurrentVersionExpiration>"
        ));
        assert!(body.contains(
            "<Expiration><ExpiredObjectDeleteMarker>true</ExpiredObjectDeleteMarker></Expiration>"
        ));
        assert!(!body.contains("<Expiration><Days>"));
        assert!(!body.contains("<Expiration><Date>"));
    }

    #[tokio::test]
    async fn get_then_set_bucket_lifecycle_preserves_marker_cleanup() {
        let get_response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Rule>
    <ID>noncurrent-marker</ID>
    <Status>Enabled</Status>
    <Filter><Prefix></Prefix></Filter>
    <NoncurrentVersionExpiration><NoncurrentDays>1</NoncurrentDays></NoncurrentVersionExpiration>
    <Expiration><ExpiredObjectDeleteMarker>true</ExpiredObjectDeleteMarker></Expiration>
  </Rule>
</LifecycleConfiguration>"#,
            ))
            .expect("build get lifecycle response");
        let (read_client, _read_request_receiver) = test_s3_client(Some(get_response));
        let rules = ObjectStore::get_bucket_lifecycle(&read_client, "bucket")
            .await
            .expect("get lifecycle rules");
        assert_eq!(rules[0].expired_object_delete_marker, Some(true));

        let set_response = http::Response::builder()
            .status(200)
            .body(SdkBody::empty())
            .expect("build set lifecycle response");
        let (write_client, write_request_receiver) = test_s3_client(Some(set_response));
        ObjectStore::set_bucket_lifecycle(&write_client, "bucket", rules)
            .await
            .expect("write lifecycle rules back");

        let request = write_request_receiver.expect_request();
        let body = request.body().bytes().expect("request body bytes");
        let body = std::str::from_utf8(body).expect("request body is utf8");
        assert!(body.contains(
            "<Expiration><ExpiredObjectDeleteMarker>true</ExpiredObjectDeleteMarker></Expiration>"
        ));
    }

    #[tokio::test]
    async fn set_bucket_lifecycle_rejects_invalid_marker_cleanup_combinations() {
        let (client, request_receiver) = test_s3_client(None);
        let mut tags = HashMap::new();
        tags.insert("env".to_string(), "prod".to_string());
        let rules = vec![
            LifecycleRule {
                id: "days-marker".to_string(),
                status: rc_core::LifecycleRuleStatus::Enabled,
                prefix: None,
                tags: None,
                expiration: Some(rc_core::LifecycleExpiration {
                    days: Some(30),
                    date: None,
                }),
                transition: None,
                noncurrent_version_expiration: None,
                noncurrent_version_transition: None,
                abort_incomplete_multipart_upload_days: None,
                expired_object_delete_marker: Some(true),
            },
            LifecycleRule {
                id: "tags-marker".to_string(),
                status: rc_core::LifecycleRuleStatus::Enabled,
                prefix: None,
                tags: Some(tags),
                expiration: None,
                transition: None,
                noncurrent_version_expiration: None,
                noncurrent_version_transition: None,
                abort_incomplete_multipart_upload_days: None,
                expired_object_delete_marker: Some(true),
            },
        ];

        for rule in rules {
            let result = ObjectStore::set_bucket_lifecycle(&client, "bucket", vec![rule]).await;
            assert!(matches!(result, Err(Error::InvalidPath(_))));
        }
        request_receiver.expect_no_request();
    }

    #[test]
    fn bucket_policy_error_kind_uses_error_code() {
        assert_eq!(
            S3Client::bucket_policy_error_kind(Some("NoSuchBucketPolicy"), Some(404), ""),
            BucketPolicyErrorKind::MissingPolicy
        );
        assert_eq!(
            S3Client::bucket_policy_error_kind(Some("NoSuchBucket"), Some(404), ""),
            BucketPolicyErrorKind::MissingBucket
        );
    }

    #[test]
    fn bucket_policy_error_kind_prefers_bucket_not_found_over_404_fallback() {
        assert_eq!(
            S3Client::bucket_policy_error_kind(None, Some(404), "NoSuchBucket"),
            BucketPolicyErrorKind::MissingBucket
        );
        assert_eq!(
            S3Client::bucket_policy_error_kind(None, Some(404), "no details"),
            BucketPolicyErrorKind::MissingPolicy
        );
    }

    #[test]
    fn bucket_policy_error_mapping_returns_expected_result() {
        let get_missing_policy = S3Client::map_get_bucket_policy_error(
            "bucket",
            BucketPolicyErrorKind::MissingPolicy,
            "NoSuchPolicy",
        )
        .expect("missing policy should map to Ok(None)");
        assert!(get_missing_policy.is_none());

        match S3Client::map_get_bucket_policy_error(
            "bucket",
            BucketPolicyErrorKind::MissingBucket,
            "NoSuchBucket",
        ) {
            Err(Error::NotFound(message)) => assert!(message.contains("Bucket not found")),
            other => panic!("Expected NotFound for missing bucket, got: {:?}", other),
        }

        let delete_missing_policy = S3Client::map_delete_bucket_policy_error(
            "bucket",
            BucketPolicyErrorKind::MissingPolicy,
            "NoSuchPolicy",
        );
        assert!(
            delete_missing_policy.is_ok(),
            "Missing policy should be treated as successful delete"
        );
    }

    #[test]
    fn notification_filter_round_trip_prefix_and_suffix() {
        let filter = S3Client::build_notification_filter(Some("logs/"), Some(".json"))
            .expect("filter should be built");
        let (prefix, suffix) = S3Client::extract_notification_filter(Some(&filter));
        assert_eq!(prefix.as_deref(), Some("logs/"));
        assert_eq!(suffix.as_deref(), Some(".json"));
    }

    #[test]
    fn notification_filter_none_when_empty() {
        assert!(S3Client::build_notification_filter(None, None).is_none());
    }

    #[test]
    fn notifications_equivalent_ignores_order_and_duplicate_events() {
        let expected = vec![
            BucketNotification {
                id: Some("a".to_string()),
                target: NotificationTarget::Queue,
                arn: "arn:aws:sqs:us-east-1:123456789012:q".to_string(),
                events: vec![
                    "s3:ObjectCreated:*".to_string(),
                    "s3:ObjectCreated:*".to_string(),
                ],
                prefix: Some("images/".to_string()),
                suffix: Some(".jpg".to_string()),
            },
            BucketNotification {
                id: Some("b".to_string()),
                target: NotificationTarget::Topic,
                arn: "arn:aws:sns:us-east-1:123456789012:t".to_string(),
                events: vec!["s3:ObjectRemoved:*".to_string()],
                prefix: None,
                suffix: None,
            },
        ];

        let actual = vec![
            BucketNotification {
                id: None,
                target: NotificationTarget::Topic,
                arn: "arn:aws:sns:us-east-1:123456789012:t".to_string(),
                events: vec!["s3:ObjectRemoved:*".to_string()],
                prefix: None,
                suffix: None,
            },
            BucketNotification {
                id: None,
                target: NotificationTarget::Queue,
                arn: "arn:aws:sqs:us-east-1:123456789012:q".to_string(),
                events: vec!["s3:ObjectCreated:*".to_string()],
                prefix: Some("images/".to_string()),
                suffix: Some(".jpg".to_string()),
            },
        ];

        assert!(S3Client::notifications_equivalent(&expected, &actual));
    }

    #[test]
    fn sdk_cors_rule_to_core_preserves_optional_fields() {
        let sdk_rule = aws_sdk_s3::types::CorsRule::builder()
            .id("web-app")
            .allowed_origins("https://app.example.com")
            .allowed_methods("get")
            .allowed_headers("Authorization")
            .expose_headers("ETag")
            .max_age_seconds(300)
            .build()
            .expect("build cors rule");

        let rule = sdk_cors_rule_to_core(&sdk_rule);
        assert_eq!(rule.id.as_deref(), Some("web-app"));
        assert_eq!(
            rule.allowed_origins,
            vec!["https://app.example.com".to_string()]
        );
        assert_eq!(rule.allowed_methods, vec!["get".to_string()]);
        assert_eq!(
            rule.allowed_headers,
            Some(vec!["Authorization".to_string()])
        );
        assert_eq!(rule.expose_headers, Some(vec!["ETag".to_string()]));
        assert_eq!(rule.max_age_seconds, Some(300));
    }

    #[test]
    fn bucket_encryption_rule_maps_sse_s3() {
        let value = aws_sdk_s3::types::ServerSideEncryptionByDefault::builder()
            .sse_algorithm(aws_sdk_s3::types::ServerSideEncryption::Aes256)
            .build()
            .expect("build rule");

        let encryption = sdk_bucket_encryption_to_core(&value).expect("map rule");
        assert_eq!(encryption, BucketEncryption::SseS3);
    }

    #[test]
    fn bucket_encryption_rule_maps_sse_kms() {
        let value = aws_sdk_s3::types::ServerSideEncryptionByDefault::builder()
            .sse_algorithm(aws_sdk_s3::types::ServerSideEncryption::AwsKms)
            .kms_master_key_id("kms-key")
            .build()
            .expect("build rule");

        let encryption = sdk_bucket_encryption_to_core(&value).expect("map rule");
        assert_eq!(
            encryption,
            BucketEncryption::SseKms {
                key_id: Some("kms-key".to_string()),
            }
        );
    }

    #[test]
    fn bucket_encryption_rule_without_kms_key_maps_to_default_kms() {
        let value = aws_sdk_s3::types::ServerSideEncryptionByDefault::builder()
            .sse_algorithm(aws_sdk_s3::types::ServerSideEncryption::AwsKms)
            .build()
            .expect("build rule");

        let encryption = sdk_bucket_encryption_to_core(&value).expect("map default kms rule");
        assert_eq!(encryption, BucketEncryption::SseKms { key_id: None });
    }

    #[test]
    fn missing_bucket_encryption_errors_are_detected() {
        assert!(is_missing_bucket_encryption_error(
            "ServerSideEncryptionConfigurationNotFoundError"
        ));
        assert!(is_missing_bucket_encryption_error(
            "The server-side encryption configuration was not found"
        ));
        assert!(!is_missing_bucket_encryption_error("AccessDenied"));
    }

    #[test]
    fn missing_bucket_encryption_response_detects_code_and_status() {
        assert!(is_missing_bucket_encryption_response(
            Some("ServerSideEncryptionConfigurationNotFoundError"),
            Some(404),
            "service error"
        ));
        assert!(is_missing_bucket_encryption_response(
            Some("NoSuchBucketEncryption"),
            Some(404),
            "service error"
        ));
        assert!(is_missing_bucket_encryption_response(
            None,
            Some(404),
            "The server-side encryption configuration was not found"
        ));
        assert!(is_missing_bucket_encryption_response(
            None,
            None,
            "The server-side encryption configuration was not found"
        ));
        assert!(!is_missing_bucket_encryption_response(
            Some("AccessDenied"),
            Some(403),
            "access denied"
        ));
        assert!(!is_missing_bucket_encryption_response(
            None,
            Some(500),
            "The server-side encryption configuration was not found"
        ));
    }

    #[test]
    fn sdk_cors_rule_to_core_drops_empty_optional_headers() {
        let sdk_rule = aws_sdk_s3::types::CorsRule::builder()
            .allowed_origins("https://app.example.com")
            .allowed_methods("GET")
            .build()
            .expect("build cors rule");

        let rule = sdk_cors_rule_to_core(&sdk_rule);
        assert_eq!(rule.allowed_headers, None);
        assert_eq!(rule.expose_headers, None);
    }

    #[test]
    fn core_cors_rule_to_sdk_normalizes_method_case() {
        let rule = CorsRule {
            id: Some("public-read".to_string()),
            allowed_origins: vec!["*".to_string()],
            allowed_methods: vec!["get".to_string(), "post".to_string()],
            allowed_headers: Some(vec!["*".to_string()]),
            expose_headers: None,
            max_age_seconds: Some(600),
        };

        let sdk_rule = core_cors_rule_to_sdk(&rule).expect("convert cors rule");
        assert_eq!(sdk_rule.id(), Some("public-read"));
        assert_eq!(sdk_rule.allowed_origins(), ["*"]);
        assert_eq!(sdk_rule.allowed_methods(), ["GET", "POST"]);
        assert_eq!(sdk_rule.allowed_headers(), ["*"]);
        assert_eq!(sdk_rule.max_age_seconds(), Some(600));
    }

    #[tokio::test]
    async fn set_bucket_cors_sends_rule_fields() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(""))
            .expect("build put bucket cors response");
        let (client, request_receiver) = test_s3_client(Some(response));

        client
            .set_bucket_cors(
                "bucket",
                vec![CorsRule {
                    id: Some("web-app".to_string()),
                    allowed_origins: vec!["https://app.example.com".to_string()],
                    allowed_methods: vec!["GET".to_string(), "POST".to_string()],
                    allowed_headers: Some(vec!["Authorization".to_string()]),
                    expose_headers: Some(vec!["ETag".to_string()]),
                    max_age_seconds: Some(600),
                }],
            )
            .await
            .expect("set bucket cors");

        let request = request_receiver.expect_request();
        assert_eq!(request.method(), http::Method::PUT);
        assert!(
            request.uri().to_string().contains("?cors"),
            "expected bucket CORS subresource in URI: {}",
            request.uri()
        );

        let body = request.body().bytes().expect("request body bytes");
        let body = std::str::from_utf8(body).expect("request body is utf8");
        assert!(body.contains("<ID>web-app</ID>"));
        assert!(body.contains("<AllowedOrigin>https://app.example.com</AllowedOrigin>"));
        assert!(body.contains("<AllowedMethod>GET</AllowedMethod>"));
        assert!(body.contains("<AllowedMethod>POST</AllowedMethod>"));
        assert!(body.contains("<AllowedHeader>Authorization</AllowedHeader>"));
        assert!(body.contains("<ExposeHeader>ETag</ExposeHeader>"));
        assert!(body.contains("<MaxAgeSeconds>600</MaxAgeSeconds>"));
    }

    #[tokio::test]
    async fn get_bucket_encryption_sends_expected_request_shape() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<ServerSideEncryptionConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Rule>
    <ApplyServerSideEncryptionByDefault>
      <SSEAlgorithm>AES256</SSEAlgorithm>
    </ApplyServerSideEncryptionByDefault>
  </Rule>
</ServerSideEncryptionConfiguration>"#,
            ))
            .expect("build get bucket encryption response");
        let (client, request_receiver) = test_s3_client(Some(response));

        let encryption = client
            .get_bucket_encryption("bucket")
            .await
            .expect("get bucket encryption");

        assert_eq!(encryption, Some(BucketEncryption::SseS3));

        let request = request_receiver.expect_request();
        assert_eq!(request.method(), http::Method::GET);
        assert!(
            request.uri().to_string().contains("?encryption"),
            "expected bucket encryption subresource in URI: {}",
            request.uri()
        );
    }

    #[tokio::test]
    async fn get_bucket_encryption_missing_configuration_returns_none() {
        let response = http::Response::builder()
            .status(404)
            .header(
                "x-amz-error-code",
                "ServerSideEncryptionConfigurationNotFoundError",
            )
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>ServerSideEncryptionConfigurationNotFoundError</Code>
  <Message>The server-side encryption configuration was not found</Message>
</Error>"#,
            ))
            .expect("build missing bucket encryption response");
        let (client, _) = test_s3_client(Some(response));

        let encryption = client
            .get_bucket_encryption("bucket")
            .await
            .expect("missing bucket encryption should map to None");

        assert_eq!(encryption, None);
    }

    #[tokio::test]
    async fn get_bucket_encryption_missing_rule_errors() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<ServerSideEncryptionConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Rule />
</ServerSideEncryptionConfiguration>"#,
            ))
            .expect("build malformed bucket encryption response");
        let (client, _) = test_s3_client(Some(response));

        match client.get_bucket_encryption("bucket").await {
            Err(Error::General(message)) => {
                assert!(message.contains("missing bucket encryption rule"))
            }
            other => panic!("expected missing rule error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_bucket_encryption_sends_expected_request_shape() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(""))
            .expect("build put bucket encryption response");
        let (client, request_receiver) = test_s3_client(Some(response));

        client
            .set_bucket_encryption(
                "bucket",
                BucketEncryption::SseKms {
                    key_id: Some("kms-key".to_string()),
                },
            )
            .await
            .expect("set bucket encryption");

        let request = request_receiver.expect_request();
        assert_eq!(request.method(), http::Method::PUT);
        assert!(
            request.uri().to_string().contains("?encryption"),
            "expected bucket encryption subresource in URI: {}",
            request.uri()
        );

        let body = request.body().bytes().expect("request body bytes");
        let body = std::str::from_utf8(body).expect("request body is utf8");
        assert!(body.contains("<ServerSideEncryptionConfiguration"));
        assert!(body.contains("<SSEAlgorithm>aws:kms</SSEAlgorithm>"));
        assert!(body.contains("<KMSMasterKeyID>kms-key</KMSMasterKeyID>"));
    }

    #[tokio::test]
    async fn delete_bucket_encryption_missing_configuration_is_successful() {
        let response = http::Response::builder()
            .status(404)
            .header("x-amz-error-code", "NoSuchBucketEncryption")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>NoSuchBucketEncryption</Code>
  <Message>The server-side encryption configuration was not found</Message>
</Error>"#,
            ))
            .expect("build missing bucket encryption delete response");
        let (client, _) = test_s3_client(Some(response));

        client
            .delete_bucket_encryption("bucket")
            .await
            .expect("missing bucket encryption should be treated as successful delete");
    }

    #[tokio::test]
    async fn delete_bucket_encryption_sends_expected_request_shape() {
        let response = http::Response::builder()
            .status(204)
            .body(SdkBody::from(""))
            .expect("build delete bucket encryption response");
        let (client, request_receiver) = test_s3_client(Some(response));

        client
            .delete_bucket_encryption("bucket")
            .await
            .expect("delete bucket encryption");

        let request = request_receiver.expect_request();
        assert_eq!(request.method(), http::Method::DELETE);
        assert!(
            request.uri().to_string().contains("?encryption"),
            "expected bucket encryption subresource in URI: {}",
            request.uri()
        );
    }

    #[test]
    fn core_cors_rule_to_sdk_drops_empty_optional_headers() {
        let rule = CorsRule {
            id: None,
            allowed_origins: vec!["https://app.example.com".to_string()],
            allowed_methods: vec!["GET".to_string()],
            allowed_headers: Some(Vec::new()),
            expose_headers: Some(Vec::new()),
            max_age_seconds: None,
        };

        let sdk_rule = core_cors_rule_to_sdk(&rule).expect("convert cors rule");
        assert!(sdk_rule.allowed_headers().is_empty());
        assert!(sdk_rule.expose_headers().is_empty());
    }

    #[test]
    fn parse_cors_configuration_xml_round_trips_rule_fields() {
        let body = r#"
<CORSConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <CORSRule>
    <ID>mc-rule</ID>
    <AllowedOrigin>https://console.example.com</AllowedOrigin>
    <AllowedMethod>GET</AllowedMethod>
    <AllowedMethod>POST</AllowedMethod>
    <AllowedHeader>*</AllowedHeader>
    <ExposeHeader>ETag</ExposeHeader>
    <MaxAgeSeconds>1200</MaxAgeSeconds>
  </CORSRule>
</CORSConfiguration>
"#;

        let rules = parse_cors_configuration_xml(body).expect("parse cors xml");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id.as_deref(), Some("mc-rule"));
        assert_eq!(
            rules[0].allowed_origins,
            vec!["https://console.example.com".to_string()]
        );
        assert_eq!(
            rules[0].allowed_methods,
            vec!["GET".to_string(), "POST".to_string()]
        );
        assert_eq!(rules[0].allowed_headers, Some(vec!["*".to_string()]));
        assert_eq!(rules[0].expose_headers, Some(vec!["ETag".to_string()]));
        assert_eq!(rules[0].max_age_seconds, Some(1200));
    }

    #[test]
    fn cors_url_uses_path_style_bucket_and_query() {
        let (client, _) = test_s3_client(None);

        let url = client.cors_url("bucket-name").expect("build cors url");

        assert_eq!(url.as_str(), "https://example.com/bucket-name?cors=");
    }

    #[test]
    fn cors_url_rejects_endpoints_without_path_segments() {
        let (client, _) = test_s3_client_with_endpoint("mailto:test@example.com", None);

        match client.cors_url("bucket-name") {
            Err(Error::Network(message)) => {
                assert!(message.contains("does not support path-style bucket operations"));
            }
            other => panic!("expected path-style endpoint error, got {other:?}"),
        }
    }

    #[test]
    fn missing_cors_configuration_errors_are_detected() {
        assert!(is_missing_cors_configuration_error(
            "NoSuchCORSConfiguration"
        ));
        assert!(is_missing_cors_configuration_error(
            "The CORS configuration does not exist"
        ));
        assert!(!is_missing_cors_configuration_error("AccessDenied"));
    }

    #[test]
    fn missing_cors_configuration_response_detects_code_and_status() {
        assert!(is_missing_cors_configuration_response(
            Some("NoSuchCORSConfiguration"),
            Some(404),
            "service error"
        ));
        assert!(is_missing_cors_configuration_response(
            None,
            Some(404),
            "The CORS configuration does not exist"
        ));
        assert!(!is_missing_cors_configuration_response(
            Some("AccessDenied"),
            Some(403),
            "access denied"
        ));
        assert!(!is_missing_cors_configuration_response(
            None,
            Some(404),
            "service error"
        ));
        assert!(!is_missing_cors_configuration_response(
            Some("NoSuchBucket"),
            Some(404),
            "NoSuchBucket"
        ));
        assert!(is_missing_cors_configuration_response(
            None,
            None,
            "The CORS configuration does not exist"
        ));
        assert!(!is_missing_cors_configuration_response(
            None,
            Some(500),
            "The CORS configuration does not exist"
        ));
    }

    #[tokio::test]
    async fn get_bucket_cors_missing_configuration_returns_empty_rules() {
        let response = http::Response::builder()
            .status(404)
            .header("x-amz-error-code", "NoSuchCORSConfiguration")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>NoSuchCORSConfiguration</Code>
  <Message>The CORS configuration does not exist</Message>
</Error>"#,
            ))
            .expect("build missing cors response");
        let (client, _) = test_s3_client(Some(response));

        let rules = client
            .get_bucket_cors("bucket")
            .await
            .expect("missing cors config should be treated as empty");

        assert!(rules.is_empty());
    }

    #[tokio::test]
    async fn delete_bucket_cors_missing_configuration_is_successful() {
        let response = http::Response::builder()
            .status(404)
            .header("x-amz-error-code", "NoSuchCORSConfiguration")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>NoSuchCORSConfiguration</Code>
  <Message>The CORS configuration does not exist</Message>
</Error>"#,
            ))
            .expect("build missing cors response");
        let (client, _) = test_s3_client(Some(response));

        client
            .delete_bucket_cors("bucket")
            .await
            .expect("missing cors config should be treated as successful delete");
    }

    #[tokio::test]
    async fn reqwest_connector_insecure_without_ca_bundle_succeeds() {
        // When insecure is true and no CA bundle is provided, the connector should be created.
        let connector = ReqwestConnector::new(true, None, None, None, None).await;
        assert!(
            connector.is_ok(),
            "Expected insecure connector creation to succeed"
        );
    }

    #[tokio::test]
    async fn reqwest_connector_invalid_ca_bundle_path_surfaces_error() {
        // Use an obviously invalid path (empty string) to trigger a read error.
        let result = ReqwestConnector::new(false, Some(""), None, None, None).await;
        match result {
            Err(Error::Network(msg)) => {
                assert!(
                    msg.contains("Failed to read CA bundle"),
                    "Unexpected error message: {msg}"
                );
            }
            other => panic!("Expected Error::Network for invalid path, got: {:?}", other),
        }
    }

    #[test]
    fn should_use_multipart_for_large_files() {
        assert!(S3Client::should_use_multipart(
            SINGLE_PUT_OBJECT_MAX_SIZE + 1
        ));
    }

    #[test]
    fn should_use_single_part_for_small_files() {
        assert!(!S3Client::should_use_multipart(0));
        assert!(!S3Client::should_use_multipart(1024 * 1024));
        assert!(!S3Client::should_use_multipart(
            crate::multipart::DEFAULT_PART_SIZE
        ));
        assert!(!S3Client::should_use_multipart(SINGLE_PUT_OBJECT_MAX_SIZE));
    }

    #[tokio::test]
    async fn delete_object_with_force_delete_sets_rustfs_header() {
        let (client, request_receiver) = test_s3_client(None);
        let path = RemotePath::new("test", "bucket", "key.txt");

        let _ = client
            .delete_object_with_options(
                &path,
                DeleteRequestOptions {
                    force_delete: true,
                    ..Default::default()
                },
            )
            .await;

        let request = request_receiver.expect_request();
        assert_eq!(request.headers().get("x-rustfs-force-delete"), Some("true"));
    }

    #[tokio::test]
    async fn versioned_delete_sends_version_and_only_explicit_governance_bypass() {
        let (client, request_receiver) = test_s3_client(None);
        let path = RemotePath::new("test", "bucket", "key.txt");

        let _ = client
            .delete_object_with_options(
                &path,
                DeleteRequestOptions {
                    version_id: Some("v1".to_string()),
                    bypass_governance: true,
                    force_delete: false,
                },
            )
            .await;

        let request = request_receiver.expect_request();
        assert!(request.uri().to_string().contains("versionId=v1"));
        assert_eq!(
            request.headers().get("x-amz-bypass-governance-retention"),
            Some("true")
        );

        let (default_client, default_request_receiver) = test_s3_client(None);
        let _ = default_client
            .delete_object_with_options(
                &path,
                DeleteRequestOptions {
                    version_id: Some("v1".to_string()),
                    ..Default::default()
                },
            )
            .await;
        let default_request = default_request_receiver.expect_request();
        assert!(
            default_request
                .headers()
                .get("x-amz-bypass-governance-retention")
                .is_none()
        );
    }

    #[tokio::test]
    async fn conditional_buffer_writes_and_deletes_set_precondition_headers() {
        let put_response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(""))
            .expect("build put response");
        let (put_client, put_request_receiver) = test_s3_client(Some(put_response));
        let path = RemotePath::new("test", "bucket", "key.txt");

        put_client
            .put_object_if_absent(&path, b"data".to_vec(), None)
            .await
            .expect("conditional put");
        let put_request = put_request_receiver.expect_request();
        assert_eq!(put_request.headers().get("if-none-match"), Some("*"));

        let delete_response = http::Response::builder()
            .status(204)
            .body(SdkBody::from(""))
            .expect("build delete response");
        let (delete_client, delete_request_receiver) = test_s3_client(Some(delete_response));
        delete_client
            .delete_object_if_match(&path, "etag-value")
            .await
            .expect("conditional delete");
        let delete_request = delete_request_receiver.expect_request();
        assert_eq!(delete_request.headers().get("if-match"), Some("etag-value"));
    }

    #[tokio::test]
    async fn conditional_path_writes_complete_with_precondition_headers() {
        let complete_response = || {
            http::Response::builder()
                .status(200)
                .header("content-type", "application/xml")
                .body(SdkBody::from(
                    r#"<CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>bucket</Bucket><Key>key.txt</Key><ETag>"final-etag"</ETag></CompleteMultipartUploadResult>"#,
                ))
                .expect("build multipart complete response")
        };
        let path = RemotePath::new("test", "bucket", "key.txt");
        let mut source = tempfile::NamedTempFile::new().expect("create upload source");
        source.write_all(b"path-data").expect("write upload source");

        let (path_upload_client, path_upload_replay) = test_s3_client_with_response_sequence(vec![
            multipart_create_response(),
            multipart_part_response(),
            complete_response(),
        ]);
        path_upload_client
            .put_object_from_path_if_absent(&path, source.path(), None, None, |_| {})
            .await
            .expect("conditional path upload");
        let path_upload_requests = path_upload_replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(path_upload_requests.len(), 3);
        assert_eq!(
            path_upload_requests[2].headers().get("if-none-match"),
            Some("*")
        );

        let (matched_upload_client, matched_upload_replay) =
            test_s3_client_with_response_sequence(vec![
                multipart_create_response(),
                multipart_part_response(),
                complete_response(),
            ]);
        matched_upload_client
            .put_object_from_path_if_match(
                &path,
                source.path(),
                None,
                None,
                "expected-etag",
                |_| {},
            )
            .await
            .expect("matched path upload");
        let matched_upload_requests = matched_upload_replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(matched_upload_requests.len(), 3);
        assert_eq!(
            matched_upload_requests[2].headers().get("if-match"),
            Some("expected-etag")
        );
    }

    #[tokio::test]
    async fn conditional_empty_path_write_uploads_one_empty_part() {
        let complete_response = http::Response::builder()
            .status(200)
            .header("content-type", "application/xml")
            .body(SdkBody::from(
                r#"<CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>bucket</Bucket><Key>empty.txt</Key><ETag>"final-etag"</ETag></CompleteMultipartUploadResult>"#,
            ))
            .expect("build multipart complete response");
        let (client, replay) = test_s3_client_with_response_sequence(vec![
            multipart_create_response(),
            multipart_part_response(),
            complete_response,
        ]);
        let path = RemotePath::new("test", "bucket", "empty.txt");
        let source = tempfile::NamedTempFile::new().expect("create empty upload source");

        client
            .put_object_from_path_if_absent(&path, source.path(), None, None, |_| {})
            .await
            .expect("conditional empty path upload");

        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].uri().contains("partNumber=1"));
        assert_eq!(requests[2].headers().get("if-none-match"), Some("*"));
    }

    fn multipart_create_response() -> http::Response<SdkBody> {
        http::Response::builder()
            .status(200)
            .header("content-type", "application/xml")
            .body(SdkBody::from(
                r#"<InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>bucket</Bucket><Key>key.txt</Key><UploadId>upload-id</UploadId></InitiateMultipartUploadResult>"#,
            ))
            .expect("build multipart create response")
    }

    fn multipart_part_response() -> http::Response<SdkBody> {
        http::Response::builder()
            .status(200)
            .header("etag", "\"part-etag\"")
            .body(SdkBody::empty())
            .expect("build multipart part response")
    }

    fn test_object_retention(mode: RetentionMode) -> ObjectRetention {
        ObjectRetention {
            mode,
            retain_until: Timestamp::from_second(4_102_444_800)
                .expect("valid 2100 retention timestamp"),
        }
    }

    #[tokio::test]
    async fn multipart_object_lock_headers_are_sent_only_during_create() {
        let complete_response = http::Response::builder()
            .status(200)
            .header("content-type", "application/xml")
            .body(SdkBody::from(
                r#"<CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>bucket</Bucket><Key>key.txt</Key><ETag>"final-etag"</ETag></CompleteMultipartUploadResult>"#,
            ))
            .expect("build multipart complete response");
        let (client, replay) = test_s3_client_with_response_sequence(vec![
            multipart_create_response(),
            multipart_part_response(),
            complete_response,
        ]);
        let path = RemotePath::new("test", "bucket", "key.txt");
        let mut source = tempfile::NamedTempFile::new().expect("create multipart source");
        source.write_all(b"data").expect("write multipart source");
        let write = ObjectWriteOptions {
            retention: Some(test_object_retention(RetentionMode::Governance)),
            legal_hold: Some(LegalHoldStatus::On),
            ..ObjectWriteOptions::default()
        };

        client
            .put_object_multipart_from_path(
                &path,
                source.path(),
                4,
                PathUploadOptions {
                    write: &write,
                    precondition: ObjectWritePrecondition::None,
                },
                |_| {},
            )
            .await
            .expect("upload locked multipart object");

        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[0].headers().get("x-amz-object-lock-mode"),
            Some("GOVERNANCE")
        );
        assert_eq!(
            requests[0]
                .headers()
                .get("x-amz-object-lock-retain-until-date"),
            Some("2100-01-01T00:00:00Z")
        );
        assert_eq!(
            requests[0].headers().get("x-amz-object-lock-legal-hold"),
            Some("ON")
        );
        for request in &requests[1..] {
            assert!(request.headers().get("x-amz-object-lock-mode").is_none());
            assert!(
                request
                    .headers()
                    .get("x-amz-object-lock-retain-until-date")
                    .is_none()
            );
            assert!(
                request
                    .headers()
                    .get("x-amz-object-lock-legal-hold")
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn multipart_path_upload_applies_sse_customer_headers_to_create_and_parts() {
        let complete_response = http::Response::builder()
            .status(200)
            .header("content-type", "application/xml")
            .body(SdkBody::from(
                r#"<CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>bucket</Bucket><Key>key.txt</Key><ETag>"final-etag"</ETag></CompleteMultipartUploadResult>"#,
            ))
            .expect("build multipart complete response");
        let (client, replay) = test_s3_client_with_response_sequence(vec![
            multipart_create_response(),
            multipart_part_response(),
            complete_response,
        ]);
        let path = RemotePath::new("test", "bucket", "key.txt");
        let mut source = tempfile::NamedTempFile::new().expect("create multipart source");
        source.write_all(b"data").expect("write multipart source");
        let write = ObjectWriteOptions {
            encryption: Some(ObjectWriteEncryption::SseCustomer {
                key: test_sse_customer_key(),
            }),
            ..ObjectWriteOptions::default()
        };

        client
            .put_object_multipart_from_path(
                &path,
                source.path(),
                4,
                PathUploadOptions {
                    write: &write,
                    precondition: ObjectWritePrecondition::None,
                },
                |_| {},
            )
            .await
            .expect("upload SSE-C multipart object");

        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        for request in &requests[..2] {
            assert_eq!(
                request
                    .headers()
                    .get("x-amz-server-side-encryption-customer-algorithm"),
                Some("AES256")
            );
            assert_eq!(
                request
                    .headers()
                    .get("x-amz-server-side-encryption-customer-key-md5"),
                Some("hRasmdxgYDKV3nvbahU1MA==")
            );
        }
        assert!(
            requests[2]
                .headers()
                .get("x-amz-server-side-encryption-customer-key")
                .is_none()
        );
    }

    #[tokio::test]
    async fn multipart_path_upload_streams_and_verifies_composite_sha256() {
        let part_raw = Sha256::digest(b"data");
        let part_checksum = BASE64_STANDARD.encode(part_raw);
        let aggregate = format!("{}-1", BASE64_STANDARD.encode(Sha256::digest(part_raw)));
        let part_response = http::Response::builder()
            .status(200)
            .header("etag", "\"part-etag\"")
            .header("x-amz-checksum-sha256", &part_checksum)
            .body(SdkBody::empty())
            .expect("build checksum part response");
        let complete_response = http::Response::builder()
            .status(200)
            .header("content-type", "application/xml")
            .header("x-amz-version-id", "multipart-v1")
            .body(SdkBody::from(
                r#"<CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>bucket</Bucket><Key>key.txt</Key><ETag>"final-etag"</ETag></CompleteMultipartUploadResult>"#,
            ))
            .expect("build checksum complete response");
        let head_response = http::Response::builder()
            .status(200)
            .header("content-length", "4")
            .header("x-amz-checksum-sha256", &aggregate)
            .header("x-amz-checksum-type", "COMPOSITE")
            .body(SdkBody::empty())
            .expect("build composite head response");
        let (client, replay) = test_s3_client_with_response_sequence(vec![
            multipart_create_response(),
            part_response,
            complete_response,
            head_response,
        ]);
        let path = RemotePath::new("test", "bucket", "key.txt");
        let mut source = tempfile::NamedTempFile::new().expect("create multipart source");
        source.write_all(b"data").expect("write multipart source");
        let write = ObjectWriteOptions {
            checksum: Some(ChecksumRequest::Calculate(ChecksumAlgorithm::Sha256)),
            ..ObjectWriteOptions::default()
        };

        client
            .put_object_multipart_from_path(
                &path,
                source.path(),
                4,
                PathUploadOptions {
                    write: &write,
                    precondition: ObjectWritePrecondition::None,
                },
                |_| {},
            )
            .await
            .expect("upload and verify multipart checksum");

        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests[0].headers().get("x-amz-checksum-algorithm"),
            Some("SHA256")
        );
        assert_eq!(
            requests[1].headers().get("x-amz-checksum-sha256"),
            Some(part_checksum.as_str())
        );
        let completion_body = requests[2].body().bytes().expect("completion request body");
        let completion_body =
            std::str::from_utf8(completion_body).expect("completion body is utf8");
        assert!(completion_body.contains("<ChecksumSHA256>"));
        assert!(completion_body.contains(&part_checksum));
        assert_eq!(
            requests[3].headers().get("x-amz-checksum-mode"),
            Some("ENABLED")
        );
        assert!(requests[3].uri().contains("versionId=multipart-v1"));
    }

    #[tokio::test]
    async fn multipart_path_upload_rejects_precomputed_checksum_before_mutation() {
        let (client, replay) = test_s3_client_with_response_sequence(Vec::new());
        let path = RemotePath::new("test", "bucket", "key.txt");
        let mut source = tempfile::NamedTempFile::new().expect("create multipart source");
        source.write_all(b"data").expect("write multipart source");
        let write = ObjectWriteOptions {
            checksum: Some(ChecksumRequest::Precomputed(
                ObjectChecksum::new(ChecksumAlgorithm::Sha256, sha256_checksum(b"data"))
                    .expect("valid checksum"),
            )),
            ..ObjectWriteOptions::default()
        };

        let error = client
            .put_object_multipart_from_path(
                &path,
                source.path(),
                4,
                PathUploadOptions {
                    write: &write,
                    precondition: ObjectWritePrecondition::None,
                },
                |_| {},
            )
            .await
            .expect_err("precomputed multipart checksums must fail before create");

        assert!(matches!(error, Error::UnsupportedFeature(_)));
        assert_eq!(replay.actual_requests().count(), 0);
    }

    #[tokio::test]
    async fn multipart_path_upload_rejects_storage_class_before_mutation() {
        let (client, replay) = test_s3_client_with_response_sequence(Vec::new());
        let path = RemotePath::new("test", "bucket", "key.txt");
        let mut source = tempfile::NamedTempFile::new().expect("create multipart source");
        source.write_all(b"data").expect("write multipart source");
        let write = ObjectWriteOptions {
            storage_class: Some("STANDARD".to_string()),
            ..ObjectWriteOptions::default()
        };

        let error = client
            .put_object_multipart_from_path(
                &path,
                source.path(),
                4,
                PathUploadOptions {
                    write: &write,
                    precondition: ObjectWritePrecondition::None,
                },
                |_| {},
            )
            .await
            .expect_err("beta.10 multipart storage class must fail before create");

        assert!(matches!(error, Error::UnsupportedFeature(_)));
        assert_eq!(replay.actual_requests().count(), 0);
    }

    #[tokio::test]
    async fn multipart_part_checksum_rejection_maps_to_conflict_and_aborts() {
        let part_response = http::Response::builder()
            .status(400)
            .header("x-amz-error-code", "BadDigest")
            .body(SdkBody::from(
                "<Error><Code>BadDigest</Code><Message>checksum rejected</Message></Error>",
            ))
            .expect("build checksum part rejection");
        let abort_response = http::Response::builder()
            .status(204)
            .body(SdkBody::empty())
            .expect("build multipart abort response");
        let (client, replay) = test_s3_client_with_response_sequence(vec![
            multipart_create_response(),
            part_response,
            abort_response,
        ]);
        let path = RemotePath::new("test", "bucket", "key.txt");
        let mut source = tempfile::NamedTempFile::new().expect("create multipart source");
        source.write_all(b"data").expect("write multipart source");
        let write = ObjectWriteOptions {
            checksum: Some(ChecksumRequest::Calculate(ChecksumAlgorithm::Sha256)),
            ..ObjectWriteOptions::default()
        };

        let error = client
            .put_object_multipart_from_path(
                &path,
                source.path(),
                4,
                PathUploadOptions {
                    write: &write,
                    precondition: ObjectWritePrecondition::None,
                },
                |_| {},
            )
            .await
            .expect_err("part checksum rejection must abort");

        assert!(matches!(error, Error::Conflict(_)));
        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].method(), "DELETE");
    }

    #[tokio::test]
    async fn multipart_checksum_error_reports_failed_abort() {
        let part_response = http::Response::builder()
            .status(400)
            .header("x-amz-error-code", "BadDigest")
            .body(SdkBody::from(
                "<Error><Code>BadDigest</Code><Message>checksum rejected</Message></Error>",
            ))
            .expect("build checksum part rejection");
        let abort_response = http::Response::builder()
            .status(500)
            .header("x-amz-error-code", "InternalError")
            .body(SdkBody::from(
                "<Error><Code>InternalError</Code><Message>abort failed</Message></Error>",
            ))
            .expect("build multipart abort failure");
        let (client, replay) = test_s3_client_with_response_sequence(vec![
            multipart_create_response(),
            part_response,
            abort_response,
        ]);
        let path = RemotePath::new("test", "bucket", "key.txt");
        let mut source = tempfile::NamedTempFile::new().expect("create multipart source");
        source.write_all(b"data").expect("write multipart source");
        let write = ObjectWriteOptions {
            checksum: Some(ChecksumRequest::Calculate(ChecksumAlgorithm::Sha256)),
            ..ObjectWriteOptions::default()
        };

        let error = client
            .put_object_multipart_from_path(
                &path,
                source.path(),
                4,
                PathUploadOptions {
                    write: &write,
                    precondition: ObjectWritePrecondition::None,
                },
                |_| {},
            )
            .await
            .expect_err("failed checksum cleanup must be reported");

        assert!(matches!(error, Error::Conflict(_)));
        assert!(error.to_string().contains("abort: failed"));
        assert_eq!(replay.actual_requests().count(), 3);
    }

    #[tokio::test]
    async fn conditional_multipart_completion_sets_if_none_match() {
        let complete_response = http::Response::builder()
            .status(200)
            .header("content-type", "application/xml")
            .body(SdkBody::from(
                r#"<CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>bucket</Bucket><Key>key.txt</Key><ETag>"final-etag"</ETag></CompleteMultipartUploadResult>"#,
            ))
            .expect("build multipart complete response");
        let (client, replay) = test_s3_client_with_response_sequence(vec![
            multipart_create_response(),
            multipart_part_response(),
            complete_response,
        ]);
        let path = RemotePath::new("test", "bucket", "key.txt");
        let mut source = tempfile::NamedTempFile::new().expect("create multipart source");
        source.write_all(b"data").expect("write multipart source");
        let write = ObjectWriteOptions {
            attributes: Some(ObjectAttributes {
                content_type: Some("text/plain".to_string()),
                ..ObjectAttributes::default()
            }),
            ..ObjectWriteOptions::default()
        };

        client
            .put_object_multipart_from_path(
                &path,
                source.path(),
                4,
                PathUploadOptions {
                    write: &write,
                    precondition: ObjectWritePrecondition::IfAbsent,
                },
                |_| {},
            )
            .await
            .expect("complete conditional multipart upload");

        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].headers().get("if-none-match"), Some("*"));
        assert!(requests[2].uri().contains("uploadId=upload-id"));
    }

    #[tokio::test]
    async fn conditional_multipart_conflicts_are_mapped_and_aborted() {
        for (status, code) in [
            (409_u16, "ConditionalRequestConflict"),
            (412_u16, "PreconditionFailed"),
        ] {
            let complete_response = http::Response::builder()
                .status(status)
                .header("content-type", "application/xml")
                .header("x-amz-error-code", code)
                .body(SdkBody::from(format!(
                    "<Error><Code>{code}</Code><Message>conditional write failed</Message></Error>"
                )))
                .expect("build multipart conflict response");
            let abort_response = http::Response::builder()
                .status(204)
                .body(SdkBody::empty())
                .expect("build multipart abort response");
            let (client, replay) = test_s3_client_with_response_sequence(vec![
                multipart_create_response(),
                multipart_part_response(),
                complete_response,
                abort_response,
            ]);
            let path = RemotePath::new("test", "bucket", "key.txt");
            let mut source = tempfile::NamedTempFile::new().expect("create multipart source");
            source.write_all(b"data").expect("write multipart source");
            let write = ObjectWriteOptions::default();

            let result = client
                .put_object_multipart_from_path(
                    &path,
                    source.path(),
                    4,
                    PathUploadOptions {
                        write: &write,
                        precondition: ObjectWritePrecondition::IfMatch("expected-etag"),
                    },
                    |_| {},
                )
                .await;

            assert!(matches!(result, Err(Error::Conflict(_))), "status {status}");
            let requests = replay.actual_requests().collect::<Vec<_>>();
            assert_eq!(requests.len(), 4, "status {status}");
            assert_eq!(
                requests[2].headers().get("if-match"),
                Some("expected-etag"),
                "status {status}"
            );
            assert_eq!(requests[3].method(), "DELETE", "status {status}");
            assert!(
                requests[3].uri().contains("uploadId=upload-id"),
                "status {status}"
            );
        }
    }

    #[tokio::test]
    async fn conditional_multipart_service_errors_preserve_response_metadata() {
        let complete_response = http::Response::builder()
            .status(500)
            .header("content-type", "application/xml")
            .body(SdkBody::from(
                "<Error><Code>InternalError</Code><Message>conditional completion failed</Message></Error>",
            ))
            .expect("build multipart service error response");
        let abort_response = http::Response::builder()
            .status(204)
            .body(SdkBody::empty())
            .expect("build multipart abort response");
        let (client, _) = test_s3_client_with_response_sequence(vec![
            multipart_create_response(),
            multipart_part_response(),
            complete_response,
            abort_response,
        ]);
        let path = RemotePath::new("test", "bucket", "key.txt");
        let mut source = tempfile::NamedTempFile::new().expect("create multipart source");
        source.write_all(b"data").expect("write multipart source");
        let write = ObjectWriteOptions::default();

        let result = client
            .put_object_multipart_from_path(
                &path,
                source.path(),
                4,
                PathUploadOptions {
                    write: &write,
                    precondition: ObjectWritePrecondition::IfAbsent,
                },
                |_| {},
            )
            .await;

        let Err(Error::Network(message)) = result else {
            panic!("expected a network error");
        };
        assert!(message.contains("status: 500"), "{message}");
        assert!(message.contains("code: InternalError"), "{message}");
    }

    #[tokio::test]
    async fn custom_headers_are_added_before_sending_sdk_requests() {
        let (client, request_receiver) = test_s3_client_with_endpoint_and_headers(
            "https://example.com",
            None,
            vec![RequestHeader {
                name: "x-amz-bucket-encrypt-enabled".to_string(),
                value: "1".to_string(),
            }],
        );
        let path = RemotePath::new("test", "bucket", "key.txt");

        let _ = client.delete_object(&path).await;

        let request = request_receiver.expect_request();
        assert_eq!(
            request.headers().get("x-amz-bucket-encrypt-enabled"),
            Some("1")
        );
        assert!(
            request
                .headers()
                .get("authorization")
                .expect("authorization header")
                .contains("x-amz-bucket-encrypt-enabled")
        );
    }

    #[tokio::test]
    async fn write_object_to_limits_download_with_range_header() {
        let response = http::Response::builder()
            .status(206)
            .header("content-length", "3")
            .header("content-range", "bytes 0-2/8")
            .body(SdkBody::from("abc"))
            .expect("build ranged get response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "key.txt");
        let mut output = Vec::new();

        let written = client
            .write_object_to(&path, &mut output, Some(3))
            .await
            .expect("stream ranged object");

        let request = request_receiver.expect_request();
        assert_eq!(request.headers().get("range"), Some("bytes=0-2"));
        assert_eq!(written, 3);
        assert_eq!(output, b"abc");
    }

    #[tokio::test]
    async fn get_object_with_options_selects_exact_version() {
        let response = http::Response::builder()
            .status(200)
            .header("content-length", "3")
            .header("x-amz-version-id", "v1")
            .body(SdkBody::from("old"))
            .expect("build versioned get response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "key.txt");
        let options =
            ObjectReadOptions::for_version(Some("v1".to_string())).expect("valid version ID");

        let data = client
            .get_object_with_options(&path, &options)
            .await
            .expect("read exact version");

        let request = request_receiver.expect_request();
        assert!(request.uri().to_string().contains("versionId=v1"));
        assert_eq!(data, b"old");
    }

    #[tokio::test]
    async fn head_object_with_options_preserves_version_id() {
        let response = http::Response::builder()
            .status(200)
            .header("content-length", "3")
            .header("x-amz-version-id", "v1")
            .body(SdkBody::from(""))
            .expect("build versioned head response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "key.txt");
        let options =
            ObjectReadOptions::for_version(Some("v1".to_string())).expect("valid version ID");

        let info = client
            .head_object_with_options(&path, &options)
            .await
            .expect("inspect exact version");

        let request = request_receiver.expect_request();
        assert!(request.uri().to_string().contains("versionId=v1"));
        assert_eq!(info.version_id.as_deref(), Some("v1"));
    }

    #[tokio::test]
    async fn transfer_read_default_delegates_version_through_trait_object() {
        let response = http::Response::builder()
            .status(200)
            .header("content-length", "3")
            .header("x-amz-version-id", "v1")
            .body(SdkBody::from(""))
            .expect("build versioned head response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "key.txt");
        let store: &dyn ObjectStore = &client;
        let options = TransferReadOptions {
            version_id: Some("v1".to_string()),
            ..TransferReadOptions::default()
        };

        let info = store
            .head_object_with_transfer_options(&path, &options)
            .await
            .expect("legacy-compatible transfer metadata read");

        let request = request_receiver.expect_request();
        assert!(request.uri().to_string().contains("versionId=v1"));
        assert_eq!(info.version_id.as_deref(), Some("v1"));
    }

    #[tokio::test]
    async fn transfer_metadata_reads_all_attributes() {
        let response = http::Response::builder()
            .status(200)
            .header("content-length", "3")
            .header("content-type", "text/plain")
            .header("cache-control", "max-age=60")
            .header("content-disposition", "attachment")
            .header("content-encoding", "gzip")
            .header("content-language", "en")
            .header("expires", "Thu, 23 Jul 2026 08:00:00 GMT")
            .header("x-amz-meta-owner", "storage")
            .header("x-amz-storage-class", "STANDARD")
            .body(SdkBody::from(""))
            .expect("build transfer metadata response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "key.txt");
        let options = TransferReadOptions {
            version_id: Some("source-v1".to_string()),
            ..TransferReadOptions::default()
        };

        let metadata = client
            .head_object_transfer_metadata(&path, &options)
            .await
            .expect("read complete transfer metadata");

        assert_eq!(
            metadata.attributes.content_type.as_deref(),
            Some("text/plain")
        );
        assert_eq!(
            metadata.attributes.cache_control.as_deref(),
            Some("max-age=60")
        );
        assert_eq!(
            metadata.attributes.content_disposition.as_deref(),
            Some("attachment")
        );
        assert_eq!(
            metadata.attributes.content_encoding.as_deref(),
            Some("gzip")
        );
        assert_eq!(metadata.attributes.content_language.as_deref(), Some("en"));
        assert!(metadata.attributes.expires.is_some());
        assert_eq!(
            metadata.attributes.user_metadata.get("owner"),
            Some(&"storage".to_string())
        );
        assert_eq!(metadata.storage_class.as_deref(), Some("STANDARD"));
        assert!(metadata.checksums.is_empty());
        let request = request_receiver.expect_request();
        assert!(request.uri().to_string().contains("versionId=source-v1"));
    }

    #[tokio::test]
    async fn exact_version_errors_distinguish_missing_versions_and_delete_markers() {
        let missing_response = http::Response::builder()
            .status(404)
            .header("x-amz-error-code", "NoSuchVersion")
            .body(SdkBody::from(
                "<Error><Code>NoSuchVersion</Code><Message>missing</Message></Error>",
            ))
            .expect("build missing version response");
        let (missing_client, _) = test_s3_client(Some(missing_response));
        let path = RemotePath::new("test", "bucket", "key.txt");
        let options =
            ObjectReadOptions::for_version(Some("missing".to_string())).expect("valid version ID");

        assert!(matches!(
            missing_client
                .get_object_with_options(&path, &options)
                .await,
            Err(Error::VersionNotFound { .. })
        ));

        let marker_response = http::Response::builder()
            .status(405)
            .header("x-amz-error-code", "MethodNotAllowed")
            .header("x-amz-delete-marker", "true")
            .header("x-amz-version-id", "marker-v1")
            .body(SdkBody::from(
                "<Error><Code>MethodNotAllowed</Code><Message>delete marker</Message></Error>",
            ))
            .expect("build delete marker response");
        let (marker_client, _) = test_s3_client(Some(marker_response));
        let marker_options = ObjectReadOptions::for_version(Some("marker-v1".to_string()))
            .expect("valid version ID");

        assert!(matches!(
            marker_client
                .get_object_with_options(&path, &marker_options)
                .await,
            Err(Error::DeleteMarker { .. })
        ));
    }

    #[tokio::test]
    async fn exact_version_maps_generic_missing_key_responses_to_missing_version() {
        let response = http::Response::builder()
            .status(404)
            .header("x-amz-error-code", "NoSuchKey")
            .body(SdkBody::from(
                "<Error><Code>NoSuchKey</Code><Message>missing</Message></Error>",
            ))
            .expect("build generic missing response");
        let (client, _) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "key.txt");
        let options =
            ObjectReadOptions::for_version(Some("missing".to_string())).expect("valid version ID");

        assert!(matches!(
            client.get_object_with_options(&path, &options).await,
            Err(Error::VersionNotFound { .. })
        ));
    }

    #[tokio::test]
    async fn exact_version_maps_bare_unauthorized_status_to_auth() {
        let response = http::Response::builder()
            .status(401)
            .body(SdkBody::from(""))
            .expect("build unauthorized response");
        let (client, _) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "key.txt");
        let options =
            ObjectReadOptions::for_version(Some("v1".to_string())).expect("valid version ID");

        assert!(matches!(
            client.get_object_with_options(&path, &options).await,
            Err(Error::Auth(_))
        ));
    }

    #[tokio::test]
    async fn exact_version_maps_unauthorized_with_retention_text_to_auth() {
        let response = http::Response::builder()
            .status(401)
            .body(SdkBody::from("governance retention is active"))
            .expect("build unauthorized response");
        let (client, _) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "key.txt");
        let options =
            ObjectReadOptions::for_version(Some("v1".to_string())).expect("valid version ID");

        assert!(matches!(
            client.get_object_with_options(&path, &options).await,
            Err(Error::Auth(_))
        ));
    }

    #[tokio::test]
    async fn version_listing_maps_bare_forbidden_status_to_auth() {
        let response = http::Response::builder()
            .status(403)
            .body(SdkBody::from(""))
            .expect("build forbidden response");
        let (client, _) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "logs/");

        assert!(matches!(
            client.list_object_versions_page(&path, Some(1000)).await,
            Err(Error::Auth(_))
        ));
    }

    #[tokio::test]
    async fn custom_headers_are_not_required_by_presigned_urls() {
        let (client, _request_receiver) = test_s3_client_with_endpoint_and_headers(
            "https://example.com",
            None,
            vec![RequestHeader {
                name: "x-amz-bucket-encrypt-enabled".to_string(),
                value: "1".to_string(),
            }],
        );
        let path = RemotePath::new("test", "bucket", "key.txt");

        let url = client
            .presign_get(&path, 3600)
            .await
            .expect("presign get should succeed");

        assert!(!url.contains("x-amz-bucket-encrypt-enabled"));
    }

    #[tokio::test]
    async fn custom_headers_are_added_to_xml_requests_before_signing() {
        let (endpoint, request_receiver, server_handle) = start_xml_test_server();
        let (client, _sdk_request_receiver) = test_s3_client_with_endpoint_and_headers(
            &endpoint,
            None,
            vec![RequestHeader {
                name: "x-amz-bucket-encrypt-enabled".to_string(),
                value: "1".to_string(),
            }],
        );
        let url = client
            .replication_url("bucket")
            .expect("replication URL should build");

        let response = client
            .xml_request(
                Method::PUT,
                url,
                Some("application/xml"),
                Some(b"<xml/>".to_vec()),
            )
            .await
            .expect("xml request should succeed");

        assert_eq!(response, "ok");
        let request = request_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("server should capture XML request");
        assert_eq!(request.method, "PUT");
        assert_eq!(request.target, "/bucket?replication=");
        assert_eq!(
            header_value(&request.headers, "x-amz-bucket-encrypt-enabled"),
            Some("1")
        );
        assert!(
            header_value(&request.headers, "authorization")
                .expect("authorization header")
                .contains("x-amz-bucket-encrypt-enabled")
        );
        server_handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn replication_check_uses_signed_empty_s3_extension_request() {
        let (endpoint, receiver, handle) = start_replication_extension_test_server(
            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
        );
        let (client, _) = test_s3_client_with_endpoint(&endpoint, None);

        let result = client
            .check_bucket_replication_detailed("source-bucket")
            .await
            .expect("replication check should succeed");
        assert!(result.legacy_empty_response);
        assert!(result.succeeded());

        let request = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("capture replication check");
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/source-bucket?replication-check");
        assert_eq!(
            header_value(&request.headers, "x-amz-content-sha256"),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert!(
            header_value(&request.headers, "authorization")
                .expect("signed request")
                .contains("/us-east-1/s3/aws4_request")
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn replication_resync_start_encodes_options_and_preserves_server_id() {
        let body = br#"{"Targets":[{"Arn":"arn:rustfs:replication::id:dest bucket","ResetID":"server-id"}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect();
        let (endpoint, receiver, handle) = start_replication_extension_test_server(response);
        let (client, _) = test_s3_client_with_endpoint(&endpoint, None);

        let result = client
            .start_bucket_replication_resync(
                "source-bucket",
                ReplicationResyncStartOptions {
                    target_arn: Some("arn:rustfs:replication::id:dest bucket".to_string()),
                    older_than: Some(Duration::from_secs(3600)),
                    reset_id: None,
                },
            )
            .await
            .expect("start resync");

        assert_eq!(result.target_arn, "arn:rustfs:replication::id:dest bucket");
        assert_eq!(result.reset_id, "server-id");
        let request = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("capture resync start");
        assert_eq!(request.method, "PUT");
        assert_eq!(
            request.target,
            "/source-bucket?replication-reset&arn=arn%3Arustfs%3Areplication%3A%3Aid%3Adest+bucket&older-than=1h"
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn replication_resync_start_encodes_caller_reset_id() {
        let body = br#"{"Targets":[{"Arn":"arn:target","ResetID":"caller id"}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect();
        let (endpoint, receiver, handle) = start_replication_extension_test_server(response);
        let (client, _) = test_s3_client_with_endpoint(&endpoint, None);

        let result = client
            .start_bucket_replication_resync(
                "source-bucket",
                ReplicationResyncStartOptions {
                    target_arn: Some("arn:target".to_string()),
                    older_than: None,
                    reset_id: Some("caller id".to_string()),
                },
            )
            .await
            .expect("start resync with caller ID");

        assert_eq!(result.reset_id, "caller id");
        let request = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("capture resync start");
        assert_eq!(
            request.target,
            "/source-bucket?replication-reset&arn=arn%3Atarget&reset-id=caller+id"
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn replication_resync_start_does_not_retry_put_after_server_error() {
        let response =
            b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                .to_vec();
        let (endpoint, count_receiver, handle) =
            start_counting_replication_extension_test_server(response);
        let (client, _) = test_s3_client_with_endpoint(&endpoint, None);

        let result = client
            .start_bucket_replication_resync(
                "source-bucket",
                ReplicationResyncStartOptions::default(),
            )
            .await;
        let request_count = count_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("receive PUT request count");

        assert_eq!(request_count, 1, "PUT must not be retried");
        assert!(matches!(result, Err(Error::Network(_))));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn replication_resync_start_does_not_follow_redirects() {
        let response = b"HTTP/1.1 307 Temporary Redirect\r\nlocation: /redirected\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            .to_vec();
        let (endpoint, count_receiver, handle) =
            start_counting_replication_extension_test_server(response);
        let (client, _) = test_s3_client_with_endpoint(&endpoint, None);

        let result = client
            .start_bucket_replication_resync(
                "source-bucket",
                ReplicationResyncStartOptions::default(),
            )
            .await;
        let request_count = count_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("receive PUT request count");

        assert_eq!(request_count, 1, "signed PUT must not follow redirects");
        assert!(matches!(result, Err(Error::General(_))));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn replication_resync_status_preserves_partial_target_state() {
        let body = br#"{"Targets":[{"Arn":"arn:target","ResetID":"reset-1","ResetBeforeDate":"2026-07-01T00:00:00Z","StartTime":"2026-07-02T00:00:00Z","EndTime":"2026-07-02T00:01:00Z","Status":"Failed","ReplicatedCount":3,"ReplicatedSize":30,"FailedCount":2,"FailedSize":20,"Bucket":"source-bucket","Object":"last.txt","Error":"target unavailable"}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect();
        let (endpoint, receiver, handle) = start_replication_extension_test_server(response);
        let (client, _) = test_s3_client_with_endpoint(&endpoint, None);

        let status = client
            .bucket_replication_resync_status("source-bucket", Some("arn:target"))
            .await
            .expect("read status");

        assert_eq!(status.targets.len(), 1);
        let target = &status.targets[0];
        assert_eq!(target.state, ReplicationResyncState::Failed);
        assert_eq!(target.failed_count, 2);
        assert_eq!(target.current_object.as_deref(), Some("last.txt"));
        assert_eq!(target.error.as_deref(), Some("target unavailable"));
        let request = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("capture status request");
        assert_eq!(
            request.target,
            "/source-bucket?replication-reset-status&arn=arn%3Atarget"
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn replication_resync_status_retains_multiple_unfiltered_targets() {
        let body = br#"{"Targets":[{"Arn":"arn:a","ResetID":"reset-a","Status":"Pending"},{"Arn":"arn:b","ResetID":"reset-b","Status":"Completed","ReplicatedCount":2}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect();
        let (endpoint, receiver, handle) = start_replication_extension_test_server(response);
        let (client, _) = test_s3_client_with_endpoint(&endpoint, None);

        let status = client
            .bucket_replication_resync_status("source-bucket", None)
            .await
            .expect("read all target statuses");

        assert_eq!(status.targets.len(), 2);
        assert_eq!(status.targets[0].target_arn, "arn:a");
        assert_eq!(status.targets[1].target_arn, "arn:b");
        assert_eq!(status.targets[1].replicated_count, 2);
        let request = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("capture unfiltered status request");
        assert_eq!(request.target, "/source-bucket?replication-reset-status");
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn replication_resync_status_is_readable_from_fresh_clients() {
        let body = br#"{"Targets":[{"Arn":"arn:target","ResetID":"reset-1","Status":"Ongoing","ReplicatedCount":3}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect();
        let (endpoint, handle) = start_repeated_replication_extension_test_server(response, 2);

        let (first_client, _) = test_s3_client_with_endpoint(&endpoint, None);
        let first = first_client
            .bucket_replication_resync_status("source-bucket", None)
            .await
            .expect("first client reads persisted status");
        drop(first_client);

        let (fresh_client, _) = test_s3_client_with_endpoint(&endpoint, None);
        let fresh = fresh_client
            .bucket_replication_resync_status("source-bucket", None)
            .await
            .expect("fresh client reads persisted status");

        assert_eq!(fresh, first);
        assert_eq!(fresh.targets[0].state, ReplicationResyncState::Ongoing);
        handle.join().expect("server thread should finish");
    }

    #[test]
    fn replication_resync_status_preserves_empty_and_future_states() {
        let target = |status: &str| ReplicationResyncTargetDto {
            arn: "arn:target".to_string(),
            reset_id: "reset-1".to_string(),
            reset_before_date: None,
            start_time: None,
            end_time: None,
            status: Some(status.to_string()),
            replicated_count: Some(0),
            replicated_size: Some(0),
            failed_count: Some(0),
            failed_size: Some(0),
            bucket: None,
            object: None,
            error: None,
        };

        let empty = S3Client::convert_resync_status_target(target(""))
            .expect("empty server state is legitimate");
        let mut missing_status_target = target("Pending");
        missing_status_target.status = None;
        let missing = S3Client::convert_resync_status_target(missing_status_target)
            .expect("omitted server state uses its documented default");
        let mut empty_reset_id_target = target("Pending");
        empty_reset_id_target.reset_id.clear();
        let empty_reset_id = S3Client::convert_resync_status_target(empty_reset_id_target)
            .expect("persisted status may have an empty reset ID");
        let future = S3Client::convert_resync_status_target(target("FutureState"))
            .expect("future server state is preserved");

        assert_eq!(empty.state, ReplicationResyncState::NotStarted);
        assert_eq!(empty.server_state, "");
        assert_eq!(missing.state, ReplicationResyncState::NotStarted);
        assert_eq!(missing.server_state, "");
        assert_eq!(empty_reset_id.reset_id, "");
        assert_eq!(future.state, ReplicationResyncState::Unknown);
        assert_eq!(future.server_state, "FutureState");
    }

    #[test]
    fn replication_resync_status_rejects_negative_counters() {
        let target = ReplicationResyncTargetDto {
            arn: "arn:target".to_string(),
            reset_id: "reset-1".to_string(),
            reset_before_date: None,
            start_time: None,
            end_time: None,
            status: Some("Pending".to_string()),
            replicated_count: Some(-1),
            replicated_size: Some(0),
            failed_count: Some(0),
            failed_size: Some(0),
            bucket: None,
            object: None,
            error: None,
        };

        let error = S3Client::convert_resync_status_target(target)
            .expect_err("negative count must be malformed");
        assert!(matches!(error, Error::General(_)));
    }

    #[tokio::test]
    async fn replication_extension_rejects_declared_oversized_body() {
        let response =
            b"HTTP/1.1 200 OK\r\ncontent-length: 1048577\r\nconnection: close\r\n\r\n".to_vec();
        let (endpoint, _receiver, handle) = start_replication_extension_test_server(response);
        let (client, _) = test_s3_client_with_endpoint(&endpoint, None);

        let error = client
            .bucket_replication_resync_status("source-bucket", None)
            .await
            .expect_err("oversized response must fail");

        assert!(matches!(error, Error::General(_)));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn replication_extension_rejects_chunked_oversized_body() {
        let body = vec![b'x'; REPLICATION_EXTENSION_BODY_LIMIT as usize + 1];
        let mut response =
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n".to_vec();
        response.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
        response.extend_from_slice(&body);
        response.extend_from_slice(b"\r\n0\r\n\r\n");
        let (endpoint, _receiver, handle) = start_replication_extension_test_server(response);
        let (client, _) = test_s3_client_with_endpoint(&endpoint, None);

        let error = client
            .bucket_replication_resync_status("source-bucket", None)
            .await
            .expect_err("chunked oversized response must fail");

        assert!(matches!(error, Error::General(_)));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn replication_check_rejects_malformed_nonempty_success_body() {
        let response =
            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok".to_vec();
        let (endpoint, _receiver, handle) = start_replication_extension_test_server(response);
        let (client, _) = test_s3_client_with_endpoint(&endpoint, None);

        let error = client
            .check_bucket_replication("source-bucket")
            .await
            .expect_err("nonempty check response must fail");

        assert!(matches!(error, Error::General(_)));
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn replication_check_retains_structured_partial_and_cleanup_outcomes() {
        let body = br#"{"Status":"FAILED","ActiveMutation":true,"MutationDescription":"Writes and deletes a temporary probe.","ProbeNamespace":".rustfs.sys/replication-check/","Targets":[{"Arn":"arn:target","Bucket":"replica","Status":"FAILED","Error":"probe cleanup failed","Phases":{"Bucket":{"Status":"OK"},"Versioning":{"Status":"OK"},"ObjectLock":{"Status":"OK"},"Put":{"Status":"OK"},"DeleteMarker":{"Status":"OK"},"VersionDelete":{"Status":"OK"},"Cleanup":{"Status":"FAILED","Error":"cleanup denied"}}}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect();
        let (endpoint, _receiver, handle) = start_replication_extension_test_server(response);
        let (client, _) = test_s3_client_with_endpoint(&endpoint, None);

        let result = client
            .check_bucket_replication_detailed("source-bucket")
            .await
            .expect("structured failure is a completed check");

        assert!(!result.succeeded());
        assert!(!result.legacy_empty_response);
        assert_eq!(result.targets.len(), 1);
        assert_eq!(
            result.targets[0].phases.cleanup.status,
            ReplicationCheckPhaseState::Failed
        );
        assert_eq!(
            result.targets[0].phases.cleanup.error.as_deref(),
            Some("cleanup denied")
        );
        handle.join().expect("server thread should finish");
    }

    #[tokio::test]
    async fn legacy_replication_check_method_maps_structured_failure_to_conflict() {
        let body = br#"{"Status":"FAILED","ActiveMutation":true,"MutationDescription":"probe","ProbeNamespace":".rustfs.sys/replication-check/","Targets":[{"Arn":"arn:target","Bucket":"replica","Status":"FAILED","Error":"bucket unavailable","Phases":{"Bucket":{"Status":"FAILED","Error":"bucket unavailable"},"Versioning":{"Status":"SKIPPED"},"ObjectLock":{"Status":"SKIPPED"},"Put":{"Status":"SKIPPED"},"DeleteMarker":{"Status":"SKIPPED"},"VersionDelete":{"Status":"SKIPPED"},"Cleanup":{"Status":"OK"}}}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect();
        let (endpoint, _receiver, handle) = start_replication_extension_test_server(response);
        let (client, _) = test_s3_client_with_endpoint(&endpoint, None);

        let error = client
            .check_bucket_replication("source-bucket")
            .await
            .expect_err("legacy method must not discard structured failure");

        assert!(matches!(error, Error::Conflict(_)));
        handle.join().expect("server thread should finish");
    }

    #[test]
    fn replication_check_rejects_inconsistent_or_unsafe_structured_results() {
        let (client, _) = test_s3_client(None);
        let inconsistent = br#"{"Status":"OK","ActiveMutation":true,"MutationDescription":"probe","ProbeNamespace":".rustfs.sys/replication-check/","Targets":[{"Arn":"arn:target","Bucket":"replica","Status":"FAILED","Error":"failed","Phases":{"Bucket":{"Status":"FAILED","Error":"failed"},"Versioning":{"Status":"SKIPPED"},"ObjectLock":{"Status":"SKIPPED"},"Put":{"Status":"SKIPPED"},"DeleteMarker":{"Status":"SKIPPED"},"VersionDelete":{"Status":"SKIPPED"},"Cleanup":{"Status":"OK"}}}]}"#;
        let control_character = br#"{"Status":"FAILED","ActiveMutation":true,"MutationDescription":"probe","ProbeNamespace":".rustfs.sys/replication-check/","Targets":[{"Arn":"arn:target","Bucket":"replica","Status":"FAILED","Error":"unsafe\nline","Phases":{"Bucket":{"Status":"FAILED","Error":"failed"},"Versioning":{"Status":"SKIPPED"},"ObjectLock":{"Status":"SKIPPED"},"Put":{"Status":"SKIPPED"},"DeleteMarker":{"Status":"SKIPPED"},"VersionDelete":{"Status":"SKIPPED"},"Cleanup":{"Status":"OK"}}}]}"#;
        let unsupported_version = br#"{"Version":2,"Status":"OK","ActiveMutation":true,"MutationDescription":"probe","ProbeNamespace":".rustfs.sys/replication-check/","Targets":[{"Arn":"arn:target","Bucket":"replica","Status":"OK","Phases":{"Bucket":{"Status":"OK"},"Versioning":{"Status":"OK"},"ObjectLock":{"Status":"OK"},"Put":{"Status":"OK"},"DeleteMarker":{"Status":"OK"},"VersionDelete":{"Status":"OK"},"Cleanup":{"Status":"OK"}}}]}"#;

        for body in [
            inconsistent.as_slice(),
            control_character.as_slice(),
            unsupported_version.as_slice(),
        ] {
            assert!(matches!(
                client.parse_replication_check_response(body),
                Err(Error::General(_))
            ));
        }
    }

    #[test]
    fn replication_check_redacts_endpoint_and_signature_details() {
        let (client, _) = test_s3_client(None);
        let body = br#"{"Status":"FAILED","ActiveMutation":true,"MutationDescription":"probe","ProbeNamespace":".rustfs.sys/replication-check/","Targets":[{"Arn":"arn:target","Bucket":"replica","Status":"FAILED","Error":"request https://user:pass@example.test?X-Amz-Signature=secret failed","Phases":{"Bucket":{"Status":"FAILED","Error":"request https://example.test failed"},"Versioning":{"Status":"SKIPPED"},"ObjectLock":{"Status":"SKIPPED"},"Put":{"Status":"SKIPPED"},"DeleteMarker":{"Status":"SKIPPED"},"VersionDelete":{"Status":"SKIPPED"},"Cleanup":{"Status":"OK"}}}]}"#;

        let result = client
            .parse_replication_check_response(body)
            .expect("unsafe remote details should be replaced");
        let serialized = serde_json::to_string(&result).expect("serialize redacted result");

        assert!(serialized.contains("redacted replication check failure"));
        assert!(!serialized.contains("example.test"));
        assert!(!serialized.contains("X-Amz-Signature"));
        assert!(!serialized.contains("user:pass"));
    }

    #[test]
    fn replication_extension_maps_typed_errors_and_redacts_credentials() {
        let (client, _) = test_s3_client(None);
        let access_denied = client.map_replication_extension_error(
            reqwest::StatusCode::FORBIDDEN,
            b"<Error><Code>AccessDenied</Code><Message>access-key secret-key denied</Message></Error>",
        );
        let invalid_request = client.map_replication_extension_error(
            reqwest::StatusCode::BAD_REQUEST,
            b"<Error><Code>InvalidRequest</Code><Message>target versioning disabled</Message></Error>",
        );
        let missing = client.map_replication_extension_error(
            reqwest::StatusCode::NOT_FOUND,
            b"<Error><Code>ReplicationConfigurationNotFoundError</Code><Message>missing</Message></Error>",
        );
        let missing_bucket = client.map_replication_extension_error(
            reqwest::StatusCode::NOT_FOUND,
            b"<Error><Code>NoSuchBucket</Code><Message>missing bucket</Message></Error>",
        );
        let missing_route =
            client.map_replication_extension_error(reqwest::StatusCode::NOT_FOUND, b"not found");
        let method_not_allowed = client.map_replication_extension_error(
            reqwest::StatusCode::METHOD_NOT_ALLOWED,
            b"method not allowed",
        );
        let unsupported = client.map_replication_extension_error(
            reqwest::StatusCode::NOT_IMPLEMENTED,
            b"<Error><Code>NotImplemented</Code><Message>unsupported</Message></Error>",
        );
        let server_error = client.map_replication_extension_error(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            b"temporarily unavailable",
        );

        assert!(matches!(access_denied, Error::Auth(_)));
        let message = access_denied.to_string();
        assert!(!message.contains("access-key"));
        assert!(!message.contains("secret-key"));
        assert!(message.contains("[REDACTED]"));
        assert!(matches!(invalid_request, Error::Conflict(_)));
        assert!(
            invalid_request
                .to_string()
                .contains("target versioning disabled")
        );
        assert!(matches!(missing, Error::NotFound(_)));
        assert!(matches!(missing_bucket, Error::NotFound(_)));
        assert!(matches!(missing_route, Error::UnsupportedFeature(_)));
        assert!(matches!(method_not_allowed, Error::UnsupportedFeature(_)));
        assert!(matches!(unsupported, Error::UnsupportedFeature(_)));
        assert!(matches!(server_error, Error::Network(_)));
    }

    #[tokio::test]
    async fn delete_object_without_force_delete_omits_rustfs_header() {
        let (client, request_receiver) = test_s3_client(None);
        let path = RemotePath::new("test", "bucket", "key.txt");

        let _ = client
            .delete_object_with_options(&path, DeleteRequestOptions::default())
            .await;

        let request = request_receiver.expect_request();
        assert!(request.headers().get("x-rustfs-force-delete").is_none());
    }

    #[tokio::test]
    async fn put_object_applies_sse_s3_headers() {
        let (client, request_receiver) = test_s3_client(None);
        let path = RemotePath::new("test", "bucket", "file.txt");

        client
            .put_object(
                &path,
                b"payload".to_vec(),
                Some("text/plain"),
                Some(&ObjectEncryptionRequest::SseS3),
            )
            .await
            .expect("put object");

        let request = request_receiver.expect_request();
        assert_eq!(
            request.headers().get("x-amz-server-side-encryption"),
            Some("AES256")
        );
    }

    #[tokio::test]
    async fn put_object_preserves_returned_version_id() {
        let response = http::Response::builder()
            .status(200)
            .header("etag", "\"etag-v2\"")
            .header("x-amz-version-id", "v2")
            .body(SdkBody::from(""))
            .expect("build versioned put response");
        let (client, _) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "file.txt");

        let info = client
            .put_object(&path, b"payload".to_vec(), Some("text/plain"), None)
            .await
            .expect("put versioned object");

        assert_eq!(info.version_id.as_deref(), Some("v2"));
        assert_eq!(info.etag.as_deref(), Some("etag-v2"));
    }

    #[tokio::test]
    async fn transfer_put_default_delegates_through_object_store_trait_object() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(""))
            .expect("build put response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "file.txt");
        let store: &dyn ObjectStore = &client;
        let options = ObjectWriteOptions {
            attributes: Some(ObjectAttributes {
                content_type: Some("text/plain".to_string()),
                ..ObjectAttributes::default()
            }),
            ..ObjectWriteOptions::default()
        };

        store
            .put_object_with_options(&path, b"payload".to_vec(), &options)
            .await
            .expect("legacy-compatible transfer put");

        let request = request_receiver.expect_request();
        assert_eq!(request.headers().get("content-type"), Some("text/plain"));
    }

    #[tokio::test]
    async fn transfer_put_applies_attributes_metadata_and_tags_atomically() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(""))
            .expect("build put response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "file.txt");
        let options = ObjectWriteOptions {
            attributes: Some(ObjectAttributes {
                content_type: Some("text/plain".to_string()),
                cache_control: Some("max-age=60".to_string()),
                content_disposition: Some("attachment".to_string()),
                content_encoding: Some("gzip".to_string()),
                content_language: Some("en".to_string()),
                expires: Some(
                    jiff::Timestamp::from_second(1_774_515_600).expect("valid expiry timestamp"),
                ),
                user_metadata: HashMap::from([("owner".to_string(), "storage".to_string())]),
            }),
            tags: Some(HashMap::from([
                ("team name".to_string(), "storage/core".to_string()),
                ("owner".to_string(), "alice smith".to_string()),
            ])),
            storage_class: Some("REDUCED_REDUNDANCY".to_string()),
            ..ObjectWriteOptions::default()
        };

        let info = client
            .put_object_with_options(&path, b"payload".to_vec(), &options)
            .await
            .expect("put object with attributes and tags");
        assert_eq!(info.storage_class.as_deref(), Some("REDUCED_REDUNDANCY"));

        let request = request_receiver.expect_request();
        assert_eq!(request.headers().get("content-type"), Some("text/plain"));
        assert_eq!(request.headers().get("cache-control"), Some("max-age=60"));
        assert_eq!(
            request.headers().get("content-disposition"),
            Some("attachment")
        );
        assert_eq!(request.headers().get("content-encoding"), Some("gzip"));
        assert_eq!(request.headers().get("content-language"), Some("en"));
        assert!(request.headers().get("expires").is_some());
        assert_eq!(request.headers().get("x-amz-meta-owner"), Some("storage"));
        assert_eq!(
            request.headers().get("x-amz-tagging"),
            Some("owner=alice+smith&team+name=storage%2Fcore")
        );
        assert_eq!(
            request.headers().get("x-amz-storage-class"),
            Some("REDUCED_REDUNDANCY")
        );
    }

    #[tokio::test]
    async fn transfer_put_preserves_explicit_empty_tags_and_maps_access_denied() {
        let empty_response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(""))
            .expect("build empty-tag put response");
        let (empty_client, empty_requests) = test_s3_client(Some(empty_response));
        let path = RemotePath::new("test", "bucket", "file.txt");
        empty_client
            .put_object_with_options(
                &path,
                b"payload".to_vec(),
                &ObjectWriteOptions {
                    tags: Some(HashMap::new()),
                    ..ObjectWriteOptions::default()
                },
            )
            .await
            .expect("put object with explicit empty tags");
        let request = empty_requests.expect_request();
        assert_eq!(request.headers().get("x-amz-tagging"), Some(""));

        let denied_response = http::Response::builder()
            .status(403)
            .header("x-amz-error-code", "AccessDenied")
            .body(SdkBody::from(
                "<Error><Code>AccessDenied</Code><Message>denied</Message></Error>",
            ))
            .expect("build access denied response");
        let (denied_client, denied_requests) = test_s3_client(Some(denied_response));
        let error = denied_client
            .put_object_with_options(
                &path,
                b"payload".to_vec(),
                &ObjectWriteOptions {
                    tags: Some(HashMap::from([(
                        "owner".to_string(),
                        "storage".to_string(),
                    )])),
                    ..ObjectWriteOptions::default()
                },
            )
            .await
            .expect_err("access denied must remain typed");
        assert!(matches!(error, Error::Auth(_)));
        denied_requests.expect_request();
    }

    #[tokio::test]
    async fn transfer_put_rejects_unsupported_and_maps_invalid_storage_classes() {
        let path = RemotePath::new("test", "bucket", "file.txt");
        let (unsupported_client, unsupported_requests) = test_s3_client(None);
        let unsupported = unsupported_client
            .put_object_with_options(
                &path,
                b"payload".to_vec(),
                &ObjectWriteOptions {
                    storage_class: Some("STANDARD_IA".to_string()),
                    ..ObjectWriteOptions::default()
                },
            )
            .await
            .expect_err("label-only RustFS storage classes must fail locally");
        assert!(matches!(unsupported, Error::UnsupportedFeature(_)));
        unsupported_requests.expect_no_request();

        let (unknown_client, unknown_requests) = test_s3_client(None);
        let unknown = unknown_client
            .put_object_with_options(
                &path,
                b"payload".to_vec(),
                &ObjectWriteOptions {
                    storage_class: Some("NOT_A_CLASS".to_string()),
                    ..ObjectWriteOptions::default()
                },
            )
            .await
            .expect_err("unknown storage classes must fail locally");
        assert!(matches!(unknown, Error::InvalidPath(_)));
        unknown_requests.expect_no_request();

        let invalid_response = http::Response::builder()
            .status(400)
            .header("x-amz-error-code", "InvalidStorageClass")
            .body(SdkBody::from(
                "<Error><Code>InvalidStorageClass</Code><Message>invalid</Message></Error>",
            ))
            .expect("build invalid storage class response");
        let (invalid_client, invalid_requests) = test_s3_client(Some(invalid_response));
        let invalid = invalid_client
            .put_object_with_options(
                &path,
                b"payload".to_vec(),
                &ObjectWriteOptions {
                    storage_class: Some("STANDARD".to_string()),
                    ..ObjectWriteOptions::default()
                },
            )
            .await
            .expect_err("service storage-class rejection must remain typed");
        assert!(matches!(invalid, Error::UnsupportedFeature(_)));
        invalid_requests.expect_request();
    }

    fn test_sse_customer_key() -> SseCustomerKey {
        SseCustomerKey::new(b"0123456789abcdef0123456789abcdef".to_vec()).expect("valid SSE-C key")
    }

    #[test]
    fn sse_customer_headers_match_known_vectors() {
        let key = test_sse_customer_key();
        let headers = SseCustomerHeaders::new(&key);
        assert_eq!(
            headers.key.as_str(),
            "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
        );
        assert_eq!(headers.key_md5.as_str(), "hRasmdxgYDKV3nvbahU1MA==");
    }

    #[tokio::test]
    async fn sse_customer_put_head_and_get_send_derived_headers() {
        let put_response = http::Response::builder()
            .status(200)
            .body(SdkBody::empty())
            .expect("build SSE-C put response");
        let head_response = http::Response::builder()
            .status(200)
            .header("content-length", "7")
            .body(SdkBody::empty())
            .expect("build SSE-C head response");
        let get_response = http::Response::builder()
            .status(200)
            .header("content-length", "7")
            .body(SdkBody::from("payload"))
            .expect("build SSE-C get response");
        let (client, replay) =
            test_s3_client_with_response_sequence(vec![put_response, head_response, get_response]);
        let path = RemotePath::new("test", "bucket", "secret.bin");
        let key = test_sse_customer_key();

        client
            .put_object_with_options(
                &path,
                b"payload".to_vec(),
                &ObjectWriteOptions {
                    encryption: Some(ObjectWriteEncryption::SseCustomer { key: key.clone() }),
                    ..ObjectWriteOptions::default()
                },
            )
            .await
            .expect("put SSE-C object");
        client
            .head_object_with_transfer_options(
                &path,
                &TransferReadOptions {
                    version_id: Some("v1".to_string()),
                    customer_key: Some(key.clone()),
                    ..TransferReadOptions::default()
                },
            )
            .await
            .expect("head SSE-C object");
        let body = client
            .get_object_with_transfer_options(
                &path,
                &TransferReadOptions {
                    customer_key: Some(key),
                    ..TransferReadOptions::default()
                },
            )
            .await
            .expect("get SSE-C object");
        assert_eq!(body, b"payload");

        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        for request in &requests {
            assert_eq!(
                request
                    .headers()
                    .get("x-amz-server-side-encryption-customer-algorithm"),
                Some("AES256")
            );
            assert_eq!(
                request
                    .headers()
                    .get("x-amz-server-side-encryption-customer-key"),
                Some("MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=")
            );
            assert_eq!(
                request
                    .headers()
                    .get("x-amz-server-side-encryption-customer-key-md5"),
                Some("hRasmdxgYDKV3nvbahU1MA==")
            );
        }
        assert!(requests[1].uri().contains("versionId=v1"));
    }

    #[tokio::test]
    async fn sse_customer_service_errors_redact_all_key_forms() {
        let encoded = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";
        let response = http::Response::builder()
            .status(400)
            .header("x-amz-error-code", "InvalidRequest")
            .body(SdkBody::from(format!(
                "<Error><Code>InvalidRequest</Code><Message>0123456789abcdef0123456789abcdef {encoded} hRasmdxgYDKV3nvbahU1MA==</Message></Error>"
            )))
            .expect("build sensitive SSE-C error");
        let (client, _) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "secret.bin");

        let error = client
            .put_object_with_options(
                &path,
                b"payload".to_vec(),
                &ObjectWriteOptions {
                    encryption: Some(ObjectWriteEncryption::SseCustomer {
                        key: test_sse_customer_key(),
                    }),
                    ..ObjectWriteOptions::default()
                },
            )
            .await
            .expect_err("service failure must be redacted");
        let message = format!("{error:?} {error}");
        assert!(!message.contains("0123456789abcdef0123456789abcdef"));
        assert!(!message.contains(encoded));
        assert!(!message.contains("hRasmdxgYDKV3nvbahU1MA=="));
    }

    #[test]
    fn sha256_checksum_matches_known_vector() {
        assert_eq!(
            sha256_checksum(b"abc"),
            "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0="
        );
        let first: [u8; 32] = Sha256::digest(b"first").into();
        let second: [u8; 32] = Sha256::digest(b"second").into();
        assert_eq!(
            composite_sha256_checksum(&[first, second]),
            "LzQWAyxTThs81Y8OBSjFqPV96lhreIsEDE+rDTcFXSg=-2"
        );
        assert_ne!(
            composite_sha256_checksum(&[second, first]),
            "LzQWAyxTThs81Y8OBSjFqPV96lhreIsEDE+rDTcFXSg=-2"
        );
    }

    #[tokio::test]
    async fn transfer_put_sends_and_verifies_sha256_for_exact_version() {
        let expected = sha256_checksum(b"payload");
        let put_response = http::Response::builder()
            .status(200)
            .header("x-amz-version-id", "v1")
            .body(SdkBody::empty())
            .expect("build checksum put response");
        let head_response = http::Response::builder()
            .status(200)
            .header("content-length", "7")
            .header("x-amz-checksum-sha256", &expected)
            .header("x-amz-checksum-type", "FULL_OBJECT")
            .body(SdkBody::empty())
            .expect("build checksum head response");
        let (client, replay) =
            test_s3_client_with_response_sequence(vec![put_response, head_response]);
        let path = RemotePath::new("test", "bucket", "file.txt");

        client
            .put_object_with_options(
                &path,
                b"payload".to_vec(),
                &ObjectWriteOptions {
                    checksum: Some(ChecksumRequest::Calculate(ChecksumAlgorithm::Sha256)),
                    ..ObjectWriteOptions::default()
                },
            )
            .await
            .expect("put and verify checksum");

        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].headers().get("x-amz-checksum-sha256"),
            Some(expected.as_str())
        );
        assert_eq!(
            requests[0].headers().get("x-amz-sdk-checksum-algorithm"),
            Some("SHA256")
        );
        assert_eq!(
            requests[1].headers().get("x-amz-checksum-mode"),
            Some("ENABLED")
        );
        assert!(requests[1].uri().contains("versionId=v1"));
    }

    #[tokio::test]
    async fn transfer_put_never_succeeds_without_matching_persisted_checksum() {
        for (reported, expected_error) in [
            (None, "unsupported"),
            (Some(sha256_checksum(b"different")), "conflict"),
        ] {
            let put_response = http::Response::builder()
                .status(200)
                .body(SdkBody::empty())
                .expect("build checksum put response");
            let mut head = http::Response::builder()
                .status(200)
                .header("content-length", "7");
            if let Some(value) = reported {
                head = head
                    .header("x-amz-checksum-sha256", value)
                    .header("x-amz-checksum-type", "FULL_OBJECT");
            }
            let head_response = head
                .body(SdkBody::empty())
                .expect("build checksum head response");
            let (client, replay) =
                test_s3_client_with_response_sequence(vec![put_response, head_response]);
            let path = RemotePath::new("test", "bucket", "file.txt");

            let error = client
                .put_object_with_options(
                    &path,
                    b"payload".to_vec(),
                    &ObjectWriteOptions {
                        checksum: Some(ChecksumRequest::Calculate(ChecksumAlgorithm::Sha256)),
                        ..ObjectWriteOptions::default()
                    },
                )
                .await
                .expect_err("unverified checksum must not report success");
            match expected_error {
                "unsupported" => assert!(matches!(error, Error::UnsupportedFeature(_))),
                "conflict" => assert!(matches!(error, Error::Conflict(_))),
                _ => unreachable!("fixed test case"),
            }
            assert_eq!(replay.actual_requests().count(), 2);
        }
    }

    #[tokio::test]
    async fn checksum_service_rejections_map_to_conflict() {
        for code in ["BadDigest", "InvalidDigest"] {
            let response = http::Response::builder()
                .status(400)
                .header("x-amz-error-code", code)
                .body(SdkBody::from(format!(
                    "<Error><Code>{code}</Code><Message>checksum rejected</Message></Error>"
                )))
                .expect("build checksum rejection");
            let (client, replay) = test_s3_client_with_response_sequence(vec![response]);
            let path = RemotePath::new("test", "bucket", "file.txt");
            let checksum =
                ObjectChecksum::new(ChecksumAlgorithm::Sha256, sha256_checksum(b"payload"))
                    .expect("valid checksum");

            let error = client
                .put_object_with_options(
                    &path,
                    b"different".to_vec(),
                    &ObjectWriteOptions {
                        checksum: Some(ChecksumRequest::Precomputed(checksum)),
                        ..ObjectWriteOptions::default()
                    },
                )
                .await
                .expect_err("service checksum rejection must remain typed");
            assert!(matches!(error, Error::Conflict(_)), "code {code}");
            assert_eq!(replay.actual_requests().count(), 1);
        }
    }

    #[tokio::test]
    async fn path_checksum_service_rejection_maps_to_conflict() {
        let response = http::Response::builder()
            .status(400)
            .header("x-amz-error-code", "BadDigest")
            .body(SdkBody::from(
                "<Error><Code>BadDigest</Code><Message>checksum rejected</Message></Error>",
            ))
            .expect("build checksum rejection");
        let (client, replay) = test_s3_client_with_response_sequence(vec![response]);
        let path = RemotePath::new("test", "bucket", "file.txt");
        let mut source = tempfile::NamedTempFile::new().expect("create checksum source");
        source.write_all(b"payload").expect("write checksum source");
        let checksum =
            ObjectChecksum::new(ChecksumAlgorithm::Sha256, sha256_checksum(b"different"))
                .expect("valid checksum");

        let error = client
            .put_object_from_path_with_options(
                &path,
                source.path(),
                &ObjectWriteOptions {
                    checksum: Some(ChecksumRequest::Precomputed(checksum)),
                    ..ObjectWriteOptions::default()
                },
                |_| {},
            )
            .await
            .expect_err("path checksum rejection must remain typed");

        assert!(matches!(error, Error::Conflict(_)));
        assert_eq!(replay.actual_requests().count(), 1);
    }

    #[tokio::test]
    async fn transfer_head_accepts_valid_composite_sha256() {
        let composite = format!("{}-2", sha256_checksum(b"part digests"));
        let response = http::Response::builder()
            .status(200)
            .header("content-length", "12")
            .header("x-amz-checksum-sha256", &composite)
            .header("x-amz-checksum-type", "COMPOSITE")
            .body(SdkBody::empty())
            .expect("build composite checksum response");
        let (client, requests) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "file.txt");

        let metadata = client
            .head_object_transfer_metadata(
                &path,
                &TransferReadOptions {
                    checksum_mode: true,
                    ..TransferReadOptions::default()
                },
            )
            .await
            .expect("read composite checksum");
        assert_eq!(metadata.checksums[0].value, composite);
        let request = requests.expect_request();
        assert_eq!(
            request.headers().get("x-amz-checksum-mode"),
            Some("ENABLED")
        );
    }

    #[tokio::test]
    async fn transfer_head_rejects_checksum_type_encoding_mismatches() {
        let plain = sha256_checksum(b"payload");
        let composite = format!("{plain}-2");
        for (checksum_type, value) in [
            ("COMPOSITE", plain.as_str()),
            ("FULL_OBJECT", composite.as_str()),
        ] {
            let response = http::Response::builder()
                .status(200)
                .header("content-length", "7")
                .header("x-amz-checksum-sha256", value)
                .header("x-amz-checksum-type", checksum_type)
                .body(SdkBody::empty())
                .expect("build mismatched checksum response");
            let (client, requests) = test_s3_client(Some(response));
            let path = RemotePath::new("test", "bucket", "file.txt");

            let error = client
                .head_object_transfer_metadata(
                    &path,
                    &TransferReadOptions {
                        checksum_mode: true,
                        ..TransferReadOptions::default()
                    },
                )
                .await
                .expect_err("checksum type mismatch must not be accepted");
            assert!(matches!(error, Error::General(_)));
            requests.expect_request();
        }
    }

    #[tokio::test]
    async fn unsupported_advanced_transfers_reject_before_backend_requests() {
        let path = RemotePath::new("test", "bucket", "file.txt");

        let (put_client, put_requests) = test_s3_client(None);
        let put_store: &dyn ObjectStore = &put_client;
        let put_error = put_store
            .put_object_with_options(
                &path,
                b"payload".to_vec(),
                &ObjectWriteOptions {
                    checksum: Some(ChecksumRequest::Calculate(ChecksumAlgorithm::Sha1)),
                    ..ObjectWriteOptions::default()
                },
            )
            .await
            .expect_err("unsupported checksum must fail before mutation");
        assert!(matches!(put_error, Error::UnsupportedFeature(_)));
        put_requests.expect_no_request();

        let (checksum_copy_client, checksum_copy_requests) = test_s3_client(None);
        let checksum_copy_error = checksum_copy_client
            .copy_object_with_transfer_options(
                &path,
                &RemotePath::new("test", "bucket", "checksum-copy.txt"),
                &TransferCopyOptions {
                    destination: ObjectWriteOptions {
                        checksum: Some(ChecksumRequest::Calculate(ChecksumAlgorithm::Sha256)),
                        ..ObjectWriteOptions::default()
                    },
                    ..TransferCopyOptions::default()
                },
            )
            .await
            .expect_err("beta.10 copy checksum must fail before mutation");
        assert!(matches!(checksum_copy_error, Error::UnsupportedFeature(_)));
        checksum_copy_requests.expect_no_request();

        for options in [
            TransferCopyOptions {
                source: TransferReadOptions {
                    customer_key: Some(test_sse_customer_key()),
                    ..TransferReadOptions::default()
                },
                ..TransferCopyOptions::default()
            },
            TransferCopyOptions {
                destination: ObjectWriteOptions {
                    encryption: Some(ObjectWriteEncryption::SseCustomer {
                        key: test_sse_customer_key(),
                    }),
                    ..ObjectWriteOptions::default()
                },
                ..TransferCopyOptions::default()
            },
            TransferCopyOptions {
                source: TransferReadOptions {
                    customer_key: Some(test_sse_customer_key()),
                    ..TransferReadOptions::default()
                },
                destination: ObjectWriteOptions {
                    encryption: Some(ObjectWriteEncryption::SseCustomer {
                        key: test_sse_customer_key(),
                    }),
                    ..ObjectWriteOptions::default()
                },
                ..TransferCopyOptions::default()
            },
        ] {
            let (sse_copy_client, sse_copy_requests) = test_s3_client(None);
            let error = sse_copy_client
                .copy_object_with_transfer_options(
                    &path,
                    &RemotePath::new("test", "bucket", "sse-copy.txt"),
                    &options,
                )
                .await
                .expect_err("beta.10 SSE-C copy must fail before mutation");
            assert!(matches!(error, Error::UnsupportedFeature(_)));
            sse_copy_requests.expect_no_request();
        }

        let (copy_client, copy_requests) = test_s3_client(None);
        let copy_store: &dyn ObjectStore = &copy_client;
        let copy_error = copy_store
            .copy_object_with_transfer_options(
                &path,
                &RemotePath::new("test", "bucket", "copy.txt"),
                &TransferCopyOptions {
                    tagging_directive: Some(rc_core::TaggingDirective::Replace),
                    destination: ObjectWriteOptions {
                        tags: Some(HashMap::new()),
                        ..ObjectWriteOptions::default()
                    },
                    ..TransferCopyOptions::default()
                },
            )
            .await
            .expect_err("unsupported tag replacement must not silently degrade");
        assert!(matches!(copy_error, Error::UnsupportedFeature(_)));
        copy_requests.expect_no_request();
    }

    #[tokio::test]
    async fn sse_customer_rejects_untrusted_transport_before_backend_requests() {
        let path = RemotePath::new("test", "bucket", "secret.bin");
        for (endpoint, insecure) in [("http://example.com", false), ("https://example.com", true)] {
            let (mut client, requests) = test_s3_client_with_endpoint(endpoint, None);
            client.alias.insecure = insecure;
            let error = client
                .put_object_with_options(
                    &path,
                    b"payload".to_vec(),
                    &ObjectWriteOptions {
                        encryption: Some(ObjectWriteEncryption::SseCustomer {
                            key: test_sse_customer_key(),
                        }),
                        ..ObjectWriteOptions::default()
                    },
                )
                .await
                .expect_err("untrusted SSE-C transport must fail");

            assert!(matches!(error, Error::UnsupportedFeature(_)));
            requests.expect_no_request();
        }
    }

    #[tokio::test]
    async fn kms_diagnostic_put_uses_sse_kms_and_sensitive_body() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(""))
            .expect("build put response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "hidden-temporary-key");

        KmsDiagnosticStore::put_kms_diagnostic_object(
            &client,
            &path,
            Zeroizing::new(b"INTERNAL_PROBE_CONTENT".to_vec()),
            "kms-key",
        )
        .await
        .expect("diagnostic put should succeed");

        let request = request_receiver.expect_request();
        assert_eq!(
            request.headers().get("x-amz-server-side-encryption"),
            Some("aws:kms")
        );
        assert_eq!(
            request
                .headers()
                .get("x-amz-server-side-encryption-aws-kms-key-id"),
            Some("kms-key")
        );
        assert_eq!(
            request.body().bytes().expect("request body bytes"),
            b"INTERNAL_PROBE_CONTENT"
        );
    }

    #[tokio::test]
    async fn kms_diagnostic_get_is_bounded_and_delete_is_permanent() {
        let oversized_response = http::Response::builder()
            .status(200)
            .header("content-length", "9")
            .body(SdkBody::from("oversized"))
            .expect("build get response");
        let (get_client, get_receiver) = test_s3_client(Some(oversized_response));
        let path = RemotePath::new("test", "bucket", "hidden-temporary-key");

        let error = KmsDiagnosticStore::get_kms_diagnostic_object(&get_client, &path, 8)
            .await
            .expect_err("oversized diagnostic response should fail");
        assert!(matches!(error, Error::General(_)));
        assert!(!error.to_string().contains("hidden-temporary-key"));
        get_receiver.expect_request();

        let delete_response = http::Response::builder()
            .status(204)
            .body(SdkBody::from(""))
            .expect("build delete response");
        let (delete_client, delete_receiver) = test_s3_client(Some(delete_response));
        KmsDiagnosticStore::delete_kms_diagnostic_object(&delete_client, &path)
            .await
            .expect("diagnostic cleanup should succeed");
        let request = delete_receiver.expect_request();
        assert_eq!(request.headers().get("x-rustfs-force-delete"), Some("true"));
    }

    #[tokio::test]
    async fn kms_diagnostic_permission_errors_are_typed_and_redacted() {
        let response = http::Response::builder()
            .status(403)
            .body(SdkBody::from("SECRET_SERVER_DETAIL_MUST_NOT_APPEAR"))
            .expect("build forbidden response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "hidden-temporary-key");

        let error = KmsDiagnosticStore::put_kms_diagnostic_object(
            &client,
            &path,
            Zeroizing::new(vec![1_u8; 8]),
            "kms-key",
        )
        .await
        .expect_err("permission denial should fail");

        assert!(matches!(error, Error::Auth(_)));
        assert!(
            !error
                .to_string()
                .contains("SECRET_SERVER_DETAIL_MUST_NOT_APPEAR")
        );
        assert!(!error.to_string().contains("hidden-temporary-key"));
        request_receiver.expect_request();
    }

    #[tokio::test]
    async fn copy_object_applies_sse_kms_headers() {
        let response = http::Response::builder()
            .status(500)
            .header("x-amz-error-code", "InternalError")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>InternalError</Code>
  <Message>Something went wrong.</Message>
</Error>"#,
            ))
            .expect("build copy object response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let src = RemotePath::new("test", "bucket", "src.txt");
        let dst = RemotePath::new("test", "bucket", "dst.txt");

        let _ = client
            .copy_object(
                &src,
                &dst,
                Some(&ObjectEncryptionRequest::SseKms {
                    key_id: "kms-key".to_string(),
                }),
            )
            .await;

        let request = request_receiver.expect_request();
        assert_eq!(
            request.headers().get("x-amz-server-side-encryption"),
            Some("aws:kms")
        );
        assert_eq!(
            request
                .headers()
                .get("x-amz-server-side-encryption-aws-kms-key-id"),
            Some("kms-key")
        );
    }

    #[tokio::test]
    async fn copy_object_url_encodes_source_path() {
        let response = http::Response::builder()
            .status(500)
            .header("x-amz-error-code", "InternalError")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>InternalError</Code>
  <Message>Something went wrong.</Message>
</Error>"#,
            ))
            .expect("build copy object response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let src = RemotePath::new("test", "source-bucket", "dir one/a+b?#.txt");
        let dst = RemotePath::new("test", "destination-bucket", "dst.txt");

        let _ = client.copy_object(&src, &dst, None).await;

        let request = request_receiver.expect_request();
        assert_eq!(
            request.headers().get("x-amz-copy-source"),
            Some("source-bucket/dir%20one/a%2Bb%3F%23.txt")
        );
    }

    #[tokio::test]
    async fn copy_object_with_options_selects_url_encoded_source_version() {
        let response = http::Response::builder()
            .status(500)
            .header("x-amz-error-code", "InternalError")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>InternalError</Code>
  <Message>Something went wrong.</Message>
</Error>"#,
            ))
            .expect("build copy object response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let src = RemotePath::new("test", "source-bucket", "dir one/a+b?#.txt");
        let dst = RemotePath::new("test", "destination-bucket", "dst.txt");
        let options = CopyObjectOptions::for_source_version(Some("v 1+/=?#%".to_string()))
            .expect("valid source version ID");

        let _ = client
            .copy_object_with_options(&src, &dst, &options, None)
            .await;

        let request = request_receiver.expect_request();
        assert_eq!(
            request.headers().get("x-amz-copy-source"),
            Some("source-bucket/dir%20one/a%2Bb%3F%23.txt?versionId=v%201%2B%2F%3D%3F%23%25")
        );
    }

    #[tokio::test]
    async fn transfer_copy_sends_explicit_metadata_copy_directive() {
        let response = http::Response::builder()
            .status(500)
            .header("x-amz-error-code", "InternalError")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>InternalError</Code></Error>"#,
            ))
            .expect("build copy object response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let src = RemotePath::new("test", "source-bucket", "src.txt");
        let dst = RemotePath::new("test", "destination-bucket", "dst.txt");

        let _ = client
            .copy_object_with_transfer_options(
                &src,
                &dst,
                &TransferCopyOptions {
                    metadata_directive: Some(MetadataDirective::Copy),
                    destination: ObjectWriteOptions {
                        storage_class: Some("STANDARD".to_string()),
                        ..ObjectWriteOptions::default()
                    },
                    ..TransferCopyOptions::default()
                },
            )
            .await;

        let request = request_receiver.expect_request();
        assert_eq!(
            request.headers().get("x-amz-metadata-directive"),
            Some("COPY")
        );
        assert_eq!(
            request.headers().get("x-amz-storage-class"),
            Some("STANDARD")
        );
    }

    #[tokio::test]
    async fn transfer_put_and_copy_send_atomic_object_lock_headers() {
        for (mode, legal_hold, expected_mode, expected_hold) in [
            (
                RetentionMode::Governance,
                LegalHoldStatus::On,
                "GOVERNANCE",
                "ON",
            ),
            (
                RetentionMode::Compliance,
                LegalHoldStatus::Off,
                "COMPLIANCE",
                "OFF",
            ),
        ] {
            let response = http::Response::builder()
                .status(400)
                .header("x-amz-error-code", "InvalidRequest")
                .body(SdkBody::from(
                    r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>InvalidRequest</Code><Message>test rejection</Message></Error>"#,
                ))
                .expect("build object write response");
            let options = ObjectWriteOptions {
                retention: Some(test_object_retention(mode)),
                legal_hold: Some(legal_hold),
                ..ObjectWriteOptions::default()
            };
            let path = RemotePath::new("test", "bucket", "object.txt");

            let (put_client, put_requests) = test_s3_client(Some(response));
            let _ = put_client
                .put_object_with_options(&path, b"payload".to_vec(), &options)
                .await;
            let put = put_requests.expect_request();
            assert_eq!(
                put.headers().get("x-amz-object-lock-mode"),
                Some(expected_mode)
            );
            assert_eq!(
                put.headers().get("x-amz-object-lock-legal-hold"),
                Some(expected_hold)
            );
            assert_eq!(
                put.headers().get("x-amz-object-lock-retain-until-date"),
                Some("2100-01-01T00:00:00Z")
            );

            let copy_response = http::Response::builder()
                .status(400)
                .header("x-amz-error-code", "InvalidRequest")
                .body(SdkBody::from(
                    r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>InvalidRequest</Code><Message>test rejection</Message></Error>"#,
                ))
                .expect("build copy object response");
            let (copy_client, copy_requests) = test_s3_client(Some(copy_response));
            let _ = copy_client
                .copy_object_with_transfer_options(
                    &path,
                    &RemotePath::new("test", "bucket", "copy.txt"),
                    &TransferCopyOptions {
                        destination: options,
                        ..TransferCopyOptions::default()
                    },
                )
                .await;
            let copy = copy_requests.expect_request();
            assert_eq!(
                copy.headers().get("x-amz-object-lock-mode"),
                Some(expected_mode)
            );
            assert_eq!(
                copy.headers().get("x-amz-object-lock-legal-hold"),
                Some(expected_hold)
            );
            assert_eq!(
                copy.headers().get("x-amz-object-lock-retain-until-date"),
                Some("2100-01-01T00:00:00Z")
            );
        }
    }

    #[tokio::test]
    async fn object_lock_write_validation_and_service_errors_are_typed() {
        let path = RemotePath::new("test", "bucket", "object.txt");
        let (expired_client, expired_requests) = test_s3_client(None);
        let expired = expired_client
            .put_object_with_options(
                &path,
                b"payload".to_vec(),
                &ObjectWriteOptions {
                    retention: Some(ObjectRetention {
                        mode: RetentionMode::Governance,
                        retain_until: Timestamp::from_second(1).expect("valid expired timestamp"),
                    }),
                    ..ObjectWriteOptions::default()
                },
            )
            .await
            .expect_err("expired retention must fail before mutation");
        assert!(matches!(expired, Error::InvalidPath(_)));
        expired_requests.expect_no_request();

        for (status, code, message, expected) in [
            (
                400,
                "ObjectLockConfigurationNotFoundError",
                "configuration unavailable",
                "unsupported",
            ),
            (
                403,
                "AccessDenied",
                "not authorized to perform s3:PutObjectRetention",
                "auth",
            ),
            (
                409,
                "InvalidRequest",
                "governance retention policy denied the write",
                "governance",
            ),
            (
                409,
                "InvalidRequest",
                "compliance retention cannot be shortened",
                "compliance",
            ),
        ] {
            let response = http::Response::builder()
                .status(status)
                .header("x-amz-error-code", code)
                .body(SdkBody::from(format!(
                    "<Error><Code>{code}</Code><Message>{message}</Message></Error>"
                )))
                .expect("build object lock rejection");
            let (client, requests) = test_s3_client(Some(response));
            let error = client
                .put_object_with_options(
                    &path,
                    b"payload".to_vec(),
                    &ObjectWriteOptions {
                        legal_hold: Some(LegalHoldStatus::On),
                        ..ObjectWriteOptions::default()
                    },
                )
                .await
                .expect_err("object lock rejection must remain typed");
            match expected {
                "unsupported" => assert!(matches!(error, Error::UnsupportedFeature(_))),
                "auth" => assert!(matches!(error, Error::Auth(_))),
                "governance" => assert!(matches!(error, Error::Conflict(_))),
                "compliance" => assert!(matches!(error, Error::Conflict(_))),
                _ => unreachable!("fixed error class"),
            }
            requests.expect_request();
        }

        let missing_response = http::Response::builder()
            .status(404)
            .header("x-amz-error-code", "NoSuchVersion")
            .body(SdkBody::from(
                "<Error><Code>NoSuchVersion</Code><Message>missing source</Message></Error>",
            ))
            .expect("build missing locked-copy source response");
        let (copy_client, copy_requests) = test_s3_client(Some(missing_response));
        let source = RemotePath::new("test", "source-bucket", "missing.txt");
        let destination = RemotePath::new("test", "destination-bucket", "locked.txt");
        let error = copy_client
            .copy_object_with_transfer_options(
                &source,
                &destination,
                &TransferCopyOptions {
                    source: TransferReadOptions {
                        version_id: Some("missing-v1".to_string()),
                        ..TransferReadOptions::default()
                    },
                    destination: ObjectWriteOptions {
                        legal_hold: Some(LegalHoldStatus::On),
                        ..ObjectWriteOptions::default()
                    },
                    ..TransferCopyOptions::default()
                },
            )
            .await
            .expect_err("locked copy must preserve missing source context");
        assert!(matches!(
            error,
            Error::VersionNotFound {
                path,
                version_id
            } if path == source.to_string() && version_id == "missing-v1"
        ));
        copy_requests.expect_request();
    }

    #[tokio::test]
    async fn transfer_copy_rejects_metadata_replace_before_request() {
        let (client, request_receiver) = test_s3_client(None);
        let src = RemotePath::new("test", "source-bucket", "src.txt");
        let dst = RemotePath::new("test", "destination-bucket", "dst.txt");

        let error = client
            .copy_object_with_transfer_options(
                &src,
                &dst,
                &TransferCopyOptions {
                    metadata_directive: Some(MetadataDirective::Replace),
                    destination: ObjectWriteOptions {
                        attributes: Some(ObjectAttributes {
                            content_type: Some("text/plain".to_string()),
                            user_metadata: HashMap::from([(
                                "owner".to_string(),
                                "storage".to_string(),
                            )]),
                            ..ObjectAttributes::default()
                        }),
                        ..ObjectWriteOptions::default()
                    },
                    ..TransferCopyOptions::default()
                },
            )
            .await
            .expect_err("beta.10 cannot safely replace complete metadata");
        assert!(matches!(error, Error::UnsupportedFeature(_)));
        request_receiver.expect_no_request();

        let (empty_client, empty_requests) = test_s3_client(None);
        let empty_error = empty_client
            .copy_object_with_transfer_options(
                &src,
                &dst,
                &TransferCopyOptions {
                    metadata_directive: Some(MetadataDirective::Replace),
                    destination: ObjectWriteOptions {
                        attributes: Some(ObjectAttributes::default()),
                        ..ObjectWriteOptions::default()
                    },
                    ..TransferCopyOptions::default()
                },
            )
            .await
            .expect_err("empty metadata replacement must not use partial server semantics");
        assert!(matches!(empty_error, Error::UnsupportedFeature(_)));
        empty_requests.expect_no_request();
    }

    #[tokio::test]
    async fn copy_object_with_options_preserves_source_and_destination_versions() {
        let copy_response = http::Response::builder()
            .status(200)
            .header("content-type", "application/xml")
            .header("x-amz-version-id", "destination-v3")
            .header("x-amz-copy-source-version-id", "source-v1")
            .body(SdkBody::from(
                r#"<CopyObjectResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><ETag>"copied-etag"</ETag><LastModified>2026-07-23T00:00:00Z</LastModified></CopyObjectResult>"#,
            ))
            .expect("build copy object response");
        let head_response = http::Response::builder()
            .status(200)
            .header("content-length", "7")
            .header("etag", "\"copied-etag\"")
            .header("x-amz-version-id", "destination-v3")
            .body(SdkBody::empty())
            .expect("build head object response");
        let (client, replay) =
            test_s3_client_with_response_sequence(vec![copy_response, head_response]);
        let src = RemotePath::new("test", "source-bucket", "src.txt");
        let dst = RemotePath::new("test", "destination-bucket", "dst.txt");
        let options = CopyObjectOptions::for_source_version(Some("source-v1".to_string()))
            .expect("valid source version ID");

        let result = client
            .copy_object_with_options(&src, &dst, &options, None)
            .await
            .expect("copy exact source version");

        assert_eq!(result.version_id.as_deref(), Some("destination-v3"));
        assert_eq!(result.source_version_id.as_deref(), Some("source-v1"));
        assert_eq!(result.etag.as_deref(), Some("copied-etag"));
        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].headers().get("x-amz-copy-source"),
            Some("source-bucket/src.txt?versionId=source-v1")
        );
    }

    #[tokio::test]
    async fn copy_object_with_options_maps_missing_source_version() {
        let response = http::Response::builder()
            .status(404)
            .header("x-amz-error-code", "NoSuchVersion")
            .body(SdkBody::from(
                "<Error><Code>NoSuchVersion</Code><Message>missing</Message></Error>",
            ))
            .expect("build missing source version response");
        let (client, _) = test_s3_client(Some(response));
        let src = RemotePath::new("test", "source-bucket", "src.txt");
        let dst = RemotePath::new("test", "destination-bucket", "dst.txt");
        let options = CopyObjectOptions::for_source_version(Some("missing-v1".to_string()))
            .expect("valid source version ID");

        let result = client
            .copy_object_with_options(&src, &dst, &options, None)
            .await;

        assert!(matches!(
            result,
            Err(Error::VersionNotFound {
                version_id,
                ..
            }) if version_id == "missing-v1"
        ));
    }

    fn multipart_copy_create_response(upload_id: &str) -> http::Response<SdkBody> {
        http::Response::builder()
            .status(200)
            .header("content-type", "application/xml")
            .body(SdkBody::from(format!(
                r#"<InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>destination-bucket</Bucket><Key>dst.txt</Key><UploadId>{upload_id}</UploadId></InitiateMultipartUploadResult>"#
            )))
            .expect("build multipart copy create response")
    }

    fn multipart_copy_part_response(
        etag: &str,
        source_version_id: Option<&str>,
    ) -> http::Response<SdkBody> {
        let mut builder = http::Response::builder()
            .status(200)
            .header("content-type", "application/xml");
        if let Some(source_version_id) = source_version_id {
            builder = builder.header("x-amz-copy-source-version-id", source_version_id);
        }
        builder
            .body(SdkBody::from(format!(
                r#"<CopyPartResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><LastModified>2026-07-23T00:00:00Z</LastModified><ETag>"{etag}"</ETag></CopyPartResult>"#
            )))
            .expect("build multipart copy part response")
    }

    fn multipart_copy_complete_response() -> http::Response<SdkBody> {
        http::Response::builder()
            .status(200)
            .header("content-type", "application/xml")
            .header("x-amz-version-id", "destination-v2")
            .body(SdkBody::from(
                r#"<CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>destination-bucket</Bucket><Key>dst.txt</Key><ETag>"complete-etag"</ETag></CompleteMultipartUploadResult>"#,
            ))
            .expect("build multipart copy complete response")
    }

    fn multipart_copy_options(source_size: u64) -> MultipartCopyOptions {
        let mut options = MultipartCopyOptions::new(source_size, "source-etag")
            .expect("valid multipart copy options");
        options.source_version_id = Some("source v1+/?".to_string());
        options.preferred_part_size = Some(S3_MULTIPART_COPY_MIN_PART_SIZE);
        options.content_type = Some("application/octet-stream".to_string());
        options
            .metadata
            .insert("owner".to_string(), "copy-test".to_string());
        options
    }

    #[tokio::test]
    async fn multipart_copy_cancelled_before_start_sends_no_requests() {
        let (client, replay) = test_s3_client_with_response_sequence(vec![]);
        let src = RemotePath::new("test", "source-bucket", "src.bin");
        let dst = RemotePath::new("test", "destination-bucket", "dst.bin");
        let options = multipart_copy_options(S3_MULTIPART_COPY_MIN_PART_SIZE);
        let cancellation = MultipartCopyCancellation::new();
        cancellation.cancel();

        let error = client
            .multipart_copy(&src, &dst, &options, &cancellation, None, &|_| {})
            .await
            .expect_err("pre-cancelled copy should be interrupted");

        assert!(matches!(error, Error::Interrupted(_)));
        assert!(replay.actual_requests().next().is_none());
    }

    #[tokio::test]
    async fn multipart_transfer_default_validates_versions_and_delegates() {
        let src = RemotePath::new("test", "source-bucket", "src.bin");
        let dst = RemotePath::new("test", "destination-bucket", "dst.bin");
        let multipart = multipart_copy_options(S3_MULTIPART_COPY_MIN_PART_SIZE);
        let mismatched = TransferCopyOptions {
            source: TransferReadOptions {
                version_id: Some("different-version".to_string()),
                ..TransferReadOptions::default()
            },
            ..TransferCopyOptions::default()
        };
        let (mismatch_client, mismatch_replay) = test_s3_client_with_response_sequence(vec![]);
        let mismatch_store: &dyn ObjectStore = &mismatch_client;

        let mismatch_error = mismatch_store
            .multipart_copy_with_transfer_options(
                &src,
                &dst,
                &multipart,
                &mismatched,
                &MultipartCopyCancellation::new(),
                &|_| {},
            )
            .await
            .expect_err("mismatched versions must fail before multipart create");
        assert!(matches!(mismatch_error, Error::InvalidPath(_)));
        assert!(mismatch_replay.actual_requests().next().is_none());

        let (client, replay) = test_s3_client_with_response_sequence(vec![
            multipart_copy_create_response("transfer-upload-id"),
            multipart_copy_part_response("part-1", Some("source v1+/?")),
            multipart_copy_complete_response(),
        ]);
        let store: &dyn ObjectStore = &client;
        let transfer = TransferCopyOptions {
            source: TransferReadOptions {
                version_id: multipart.source_version_id.clone(),
                ..TransferReadOptions::default()
            },
            destination: ObjectWriteOptions {
                encryption: Some(ObjectWriteEncryption::Managed(
                    ObjectEncryptionRequest::SseS3,
                )),
                ..ObjectWriteOptions::default()
            },
            ..TransferCopyOptions::default()
        };

        let result = store
            .multipart_copy_with_transfer_options(
                &src,
                &dst,
                &multipart,
                &transfer,
                &MultipartCopyCancellation::new(),
                &|_| {},
            )
            .await
            .expect("legacy-compatible multipart transfer");

        assert_eq!(result.upload_id, "transfer-upload-id");
        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[0].headers().get("x-amz-server-side-encryption"),
            Some("AES256")
        );
    }

    #[tokio::test]
    async fn multipart_transfer_copy_preflights_and_applies_all_source_attributes() {
        let head_response = http::Response::builder()
            .status(200)
            .header("content-length", S3_MULTIPART_COPY_MIN_PART_SIZE)
            .header("content-type", "text/plain")
            .header("cache-control", "max-age=60")
            .header("content-disposition", "attachment")
            .header("content-encoding", "gzip")
            .header("content-language", "en")
            .header("expires", "Thu, 23 Jul 2026 08:00:00 GMT")
            .header("x-amz-meta-owner", "source")
            .body(SdkBody::empty())
            .expect("build source metadata response");
        let (client, replay) = test_s3_client_with_response_sequence(vec![
            head_response,
            multipart_copy_create_response("attribute-upload-id"),
            multipart_copy_part_response("part-1", Some("source v1+/?")),
            multipart_copy_complete_response(),
        ]);
        let src = RemotePath::new("test", "source-bucket", "src.bin");
        let dst = RemotePath::new("test", "destination-bucket", "dst.bin");
        let multipart = multipart_copy_options(S3_MULTIPART_COPY_MIN_PART_SIZE);
        let transfer = TransferCopyOptions {
            metadata_directive: Some(MetadataDirective::Copy),
            ..TransferCopyOptions::default()
        };

        client
            .multipart_copy_with_transfer_options(
                &src,
                &dst,
                &multipart,
                &transfer,
                &MultipartCopyCancellation::new(),
                &|_| {},
            )
            .await
            .expect("multipart copy with source metadata");

        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 4);
        assert!(
            requests[0]
                .uri()
                .to_string()
                .contains("versionId=source%20v1%2B%2F%3F")
        );
        let create = &requests[1];
        assert_eq!(create.headers().get("content-type"), Some("text/plain"));
        assert_eq!(create.headers().get("cache-control"), Some("max-age=60"));
        assert_eq!(
            create.headers().get("content-disposition"),
            Some("attachment")
        );
        assert_eq!(create.headers().get("content-encoding"), Some("gzip"));
        assert_eq!(create.headers().get("content-language"), Some("en"));
        assert!(create.headers().get("expires").is_some());
        assert_eq!(create.headers().get("x-amz-meta-owner"), Some("source"));
    }

    #[tokio::test]
    async fn multipart_transfer_rejects_server_gaps_before_preflight() {
        let (client, replay) = test_s3_client_with_response_sequence(vec![]);
        let src = RemotePath::new("test", "source-bucket", "src.bin");
        let dst = RemotePath::new("test", "destination-bucket", "dst.bin");
        let multipart = multipart_copy_options(S3_MULTIPART_COPY_MIN_PART_SIZE);
        let transfer = TransferCopyOptions {
            source: TransferReadOptions {
                version_id: multipart.source_version_id.clone(),
                ..TransferReadOptions::default()
            },
            tagging_directive: Some(rc_core::TaggingDirective::Replace),
            destination: ObjectWriteOptions {
                tags: Some(HashMap::new()),
                ..ObjectWriteOptions::default()
            },
            ..TransferCopyOptions::default()
        };

        let error = client
            .multipart_copy_with_transfer_options(
                &src,
                &dst,
                &multipart,
                &transfer,
                &MultipartCopyCancellation::new(),
                &|_| {},
            )
            .await
            .expect_err("unsupported multipart tags must fail before preflight");
        assert!(matches!(error, Error::UnsupportedFeature(_)));
        assert!(replay.actual_requests().next().is_none());

        let (storage_client, storage_replay) = test_s3_client_with_response_sequence(vec![]);
        let storage_transfer = TransferCopyOptions {
            metadata_directive: Some(MetadataDirective::Copy),
            destination: ObjectWriteOptions {
                storage_class: Some("STANDARD".to_string()),
                ..ObjectWriteOptions::default()
            },
            ..TransferCopyOptions::default()
        };
        let storage_error = storage_client
            .multipart_copy_with_transfer_options(
                &src,
                &dst,
                &multipart,
                &storage_transfer,
                &MultipartCopyCancellation::new(),
                &|_| {},
            )
            .await
            .expect_err("multipart storage class must fail before metadata preflight");
        assert!(matches!(storage_error, Error::UnsupportedFeature(_)));
        assert!(storage_replay.actual_requests().next().is_none());
    }

    #[tokio::test]
    async fn multipart_copy_sends_encoded_versioned_ranges_and_reports_progress() {
        let source_size = S3_MULTIPART_COPY_MIN_PART_SIZE + 1;
        let (client, replay) = test_s3_client_with_response_sequence(vec![
            multipart_copy_create_response("copy-upload-id"),
            multipart_copy_part_response("part-1", Some("source v1+/?")),
            multipart_copy_part_response("part-2", Some("source v1+/?")),
            multipart_copy_complete_response(),
        ]);
        let src = RemotePath::new("test", "source-bucket", "dir one/a+b?#.bin");
        let dst = RemotePath::new("test", "destination-bucket", "dst.txt");
        let options = multipart_copy_options(source_size);
        let progress = std::sync::Mutex::new(Vec::new());

        let result = client
            .multipart_copy(
                &src,
                &dst,
                &options,
                &MultipartCopyCancellation::new(),
                Some(&ObjectEncryptionRequest::SseKms {
                    key_id: "kms-key".to_string(),
                }),
                &|bytes| progress.lock().expect("progress lock").push(bytes),
            )
            .await
            .expect("multipart server-side copy");

        assert_eq!(result.upload_id, "copy-upload-id");
        assert_eq!(result.part_count, 2);
        assert_eq!(result.bytes_copied, source_size);
        assert_eq!(result.object.size_bytes, Some(source_size as i64));
        assert_eq!(result.object.version_id.as_deref(), Some("destination-v2"));
        assert_eq!(
            result.object.source_version_id.as_deref(),
            Some("source v1+/?")
        );
        assert_eq!(
            *progress.lock().expect("progress lock"),
            vec![S3_MULTIPART_COPY_MIN_PART_SIZE, source_size]
        );

        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].method(), "POST");
        assert!(requests[0].uri().ends_with("?uploads"));
        assert_eq!(
            requests[0].headers().get("content-type"),
            Some("application/octet-stream")
        );
        assert_eq!(
            requests[0].headers().get("x-amz-meta-owner"),
            Some("copy-test")
        );
        assert_eq!(
            requests[0].headers().get("x-amz-server-side-encryption"),
            Some("aws:kms")
        );
        assert_eq!(
            requests[0]
                .headers()
                .get("x-amz-server-side-encryption-aws-kms-key-id"),
            Some("kms-key")
        );

        for request in &requests[1..=2] {
            assert_eq!(request.method(), "PUT");
            assert_eq!(
                request.headers().get("x-amz-copy-source"),
                Some("source-bucket/dir%20one/a%2Bb%3F%23.bin?versionId=source%20v1%2B%2F%3F")
            );
            assert_eq!(
                request.headers().get("x-amz-copy-source-if-match"),
                Some("\"source-etag\"")
            );
        }
        assert_eq!(
            requests[1].headers().get("x-amz-copy-source-range"),
            Some("bytes=0-5242879")
        );
        assert_eq!(
            requests[2].headers().get("x-amz-copy-source-range"),
            Some("bytes=5242880-5242880")
        );
        assert_eq!(requests[3].method(), "POST");
        assert!(requests[3].uri().contains("uploadId=copy-upload-id"));
        let completion_body = requests[3].body().bytes().expect("completion request body");
        let completion_body = std::str::from_utf8(completion_body).expect("completion body is XML");
        assert!(completion_body.contains("part-1"));
        assert!(completion_body.contains("part-2"));
    }

    #[tokio::test]
    async fn multipart_copy_part_failure_aborts_once_and_preserves_missing_version() {
        let missing_version_response = http::Response::builder()
            .status(404)
            .header("content-type", "application/xml")
            .header("x-amz-error-code", "NoSuchVersion")
            .body(SdkBody::from(
                "<Error><Code>NoSuchVersion</Code><Message>missing</Message></Error>",
            ))
            .expect("build missing version response");
        let abort_response = http::Response::builder()
            .status(204)
            .body(SdkBody::empty())
            .expect("build abort response");
        let (client, replay) = test_s3_client_with_response_sequence(vec![
            multipart_copy_create_response("failed-upload-id"),
            missing_version_response,
            abort_response,
        ]);
        let src = RemotePath::new("test", "source-bucket", "src.bin");
        let dst = RemotePath::new("test", "destination-bucket", "dst.bin");
        let options = multipart_copy_options(S3_MULTIPART_COPY_MIN_PART_SIZE);

        let error = client
            .multipart_copy(
                &src,
                &dst,
                &options,
                &MultipartCopyCancellation::new(),
                None,
                &|_| {},
            )
            .await
            .expect_err("missing source version should fail");

        assert!(matches!(
            error,
            Error::VersionNotFound {
                path,
                version_id,
            } if version_id == "source v1+/?"
                && path.contains("failed-upload-id")
                && path.contains("abort: succeeded")
        ));
        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method() == "DELETE")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn multipart_copy_access_denial_stays_typed_after_abort() {
        let denied_response = http::Response::builder()
            .status(403)
            .header("content-type", "application/xml")
            .header("x-amz-error-code", "AccessDenied")
            .body(SdkBody::from("<Error><Code>AccessDenied</Code><Message>access-key secret-key custom-header-secret</Message></Error>"))
            .expect("build access denied response");
        let abort_response = http::Response::builder()
            .status(204)
            .body(SdkBody::empty())
            .expect("build abort response");
        let (client, _) = test_s3_client_with_response_sequence_and_headers(
            vec![
                multipart_copy_create_response("denied-upload-id"),
                denied_response,
                abort_response,
            ],
            vec![RequestHeader {
                name: "x-test-secret".to_string(),
                value: "custom-header-secret".to_string(),
            }],
        );
        let src = RemotePath::new("test", "source-bucket", "src.bin");
        let dst = RemotePath::new("test", "destination-bucket", "dst.bin");
        let options = multipart_copy_options(S3_MULTIPART_COPY_MIN_PART_SIZE);

        let error = client
            .multipart_copy(
                &src,
                &dst,
                &options,
                &MultipartCopyCancellation::new(),
                None,
                &|_| {},
            )
            .await
            .expect_err("access denial should fail");

        assert_eq!(error.exit_code(), 4);
        let display = error.to_string();
        assert!(matches!(error, Error::Auth(_)));
        assert!(display.contains("denied-upload-id"));
        for secret in ["access-key", "secret-key", "custom-header-secret"] {
            assert!(!display.contains(secret), "{display}");
        }
    }

    #[tokio::test]
    async fn multipart_copy_create_failure_redacts_all_configured_secrets() {
        let missing_bucket_response = http::Response::builder()
            .status(404)
            .header("content-type", "application/xml")
            .header("x-amz-error-code", "NoSuchBucket")
            .body(SdkBody::from(
                "<Error><Code>NoSuchBucket</Code><Message>missing</Message></Error>",
            ))
            .expect("build missing bucket response");
        let (client, replay) = test_s3_client_with_response_sequence_and_headers(
            vec![missing_bucket_response],
            vec![RequestHeader {
                name: "x-test-secret".to_string(),
                value: "custom-header-secret".to_string(),
            }],
        );
        let src = RemotePath::new("test", "source-bucket", "src.bin");
        let dst = RemotePath::new(
            "test",
            "access-key-secret-key-custom-header-secret",
            "dst.bin",
        );
        let options = multipart_copy_options(S3_MULTIPART_COPY_MIN_PART_SIZE);

        let error = client
            .multipart_copy(
                &src,
                &dst,
                &options,
                &MultipartCopyCancellation::new(),
                None,
                &|_| {},
            )
            .await
            .expect_err("missing destination bucket should fail");
        let display = error.to_string();

        assert!(matches!(error, Error::NotFound(_)));
        assert!(display.contains("[REDACTED]"), "{display}");
        for secret in ["access-key", "secret-key", "custom-header-secret"] {
            assert!(!display.contains(secret), "{display}");
        }
        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method(), "POST");
    }

    #[tokio::test]
    async fn multipart_copy_part_precondition_failure_aborts_once_as_conflict() {
        let conflict_response = http::Response::builder()
            .status(412)
            .header("content-type", "application/xml")
            .header("x-amz-error-code", "PreconditionFailed")
            .body(SdkBody::from(
                "<Error><Code>PreconditionFailed</Code><Message>source changed</Message></Error>",
            ))
            .expect("build precondition response");
        let abort_response = http::Response::builder()
            .status(204)
            .body(SdkBody::empty())
            .expect("build abort response");
        let (client, replay) = test_s3_client_with_response_sequence(vec![
            multipart_copy_create_response("conflict-upload-id"),
            conflict_response,
            abort_response,
        ]);
        let src = RemotePath::new("test", "source-bucket", "src.bin");
        let dst = RemotePath::new("test", "destination-bucket", "dst.bin");
        let options = multipart_copy_options(S3_MULTIPART_COPY_MIN_PART_SIZE);

        let error = client
            .multipart_copy(
                &src,
                &dst,
                &options,
                &MultipartCopyCancellation::new(),
                None,
                &|_| {},
            )
            .await
            .expect_err("source precondition should fail");

        assert_eq!(error.exit_code(), 6);
        assert!(matches!(error, Error::Conflict(_)));
        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method() == "DELETE")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn multipart_copy_missing_part_etag_aborts_once() {
        let missing_etag_response = http::Response::builder()
            .status(200)
            .header("content-type", "application/xml")
            .body(SdkBody::from(
                r#"<CopyPartResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><LastModified>2026-07-23T00:00:00Z</LastModified></CopyPartResult>"#,
            ))
            .expect("build missing ETag response");
        let abort_response = http::Response::builder()
            .status(204)
            .body(SdkBody::empty())
            .expect("build abort response");
        let (client, replay) = test_s3_client_with_response_sequence(vec![
            multipart_copy_create_response("missing-etag-upload-id"),
            missing_etag_response,
            abort_response,
        ]);
        let src = RemotePath::new("test", "source-bucket", "src.bin");
        let dst = RemotePath::new("test", "destination-bucket", "dst.bin");
        let options = multipart_copy_options(S3_MULTIPART_COPY_MIN_PART_SIZE);

        let error = client
            .multipart_copy(
                &src,
                &dst,
                &options,
                &MultipartCopyCancellation::new(),
                None,
                &|_| {},
            )
            .await
            .expect_err("missing ETag should fail");

        assert!(matches!(error, Error::General(_)));
        assert!(error.to_string().contains("missing-etag-upload-id"));
        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method() == "DELETE")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn multipart_copy_cooperative_cancellation_aborts_pending_part_once() {
        let (client, replay) = test_s3_client_with_pending_part();
        let replay_for_wait = replay.clone();
        let cancellation = MultipartCopyCancellation::new();
        let cancellation_for_copy = cancellation.clone();
        let copy = tokio::spawn(async move {
            let src = RemotePath::new("test", "source-bucket", "src.bin");
            let dst = RemotePath::new("test", "destination-bucket", "dst.bin");
            let options = multipart_copy_options(S3_MULTIPART_COPY_MIN_PART_SIZE);
            client
                .multipart_copy(&src, &dst, &options, &cancellation_for_copy, None, &|_| {})
                .await
        });

        tokio::time::timeout(
            Duration::from_secs(2),
            replay_for_wait.wait_for_pending_request(),
        )
        .await
        .expect("part request should become pending");
        cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(2), copy)
            .await
            .expect("cooperative cancellation must await abort")
            .expect("copy task")
            .expect_err("copy should be interrupted");

        assert_eq!(error.exit_code(), 130);
        assert!(matches!(error, Error::Interrupted(_)));
        assert!(error.to_string().contains("cancel-upload-id"));
        assert!(error.to_string().contains("abort: succeeded"));
        let requests = replay.requests();
        assert_eq!(requests.len(), 3, "{requests:?}");
        assert_eq!(
            requests
                .iter()
                .filter(|(method, _)| method == "DELETE")
                .count(),
            1
        );
        assert!(requests[1].1.contains("partNumber=1"));
        assert!(requests[2].1.contains("uploadId=cancel-upload-id"));
    }

    #[tokio::test]
    async fn multipart_copy_ready_completion_wins_simultaneous_cancellation() {
        let source_size = S3_MULTIPART_COPY_MIN_PART_SIZE;
        let (client, replay) = test_s3_client_with_response_sequence(vec![
            multipart_copy_create_response("complete-race-id"),
            multipart_copy_part_response("part-1", None),
            multipart_copy_complete_response(),
        ]);
        let src = RemotePath::new("test", "source-bucket", "src.bin");
        let dst = RemotePath::new("test", "destination-bucket", "dst.bin");
        let options = multipart_copy_options(source_size);
        let cancellation = MultipartCopyCancellation::new();

        let result = client
            .multipart_copy(&src, &dst, &options, &cancellation, None, &|bytes| {
                assert_eq!(bytes, source_size);
                cancellation.cancel();
            })
            .await
            .expect("ready completion should win cancellation");

        assert_eq!(result.bytes_copied, source_size);
        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method() == "DELETE")
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn multipart_copy_complete_failure_aborts_exactly_once() {
        let complete_failure = http::Response::builder()
            .status(500)
            .header("content-type", "application/xml")
            .header("x-amz-error-code", "InternalError")
            .body(SdkBody::from(
                "<Error><Code>InternalError</Code><Message>complete failed</Message></Error>",
            ))
            .expect("build complete failure response");
        let abort_response = http::Response::builder()
            .status(204)
            .body(SdkBody::empty())
            .expect("build abort response");
        let (client, replay) = test_s3_client_with_response_sequence(vec![
            multipart_copy_create_response("complete-failure-id"),
            multipart_copy_part_response("part-1", None),
            complete_failure,
            abort_response,
        ]);
        let src = RemotePath::new("test", "source-bucket", "src.bin");
        let dst = RemotePath::new("test", "destination-bucket", "dst.bin");
        let options = multipart_copy_options(S3_MULTIPART_COPY_MIN_PART_SIZE);

        let error = client
            .multipart_copy(
                &src,
                &dst,
                &options,
                &MultipartCopyCancellation::new(),
                None,
                &|_| {},
            )
            .await
            .expect_err("completion failure should abort");

        assert!(error.to_string().contains("complete-failure-id"));
        assert!(error.to_string().contains("abort: succeeded"));
        let requests = replay.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method() == "DELETE")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn multipart_copy_abort_failure_is_reported_without_service_details() {
        let part_failure = http::Response::builder()
            .status(500)
            .header("content-type", "application/xml")
            .header("x-amz-error-code", "InternalError")
            .body(SdkBody::from(
                "<Error><Code>InternalError</Code><Message>part failed</Message></Error>",
            ))
            .expect("build part failure response");
        let abort_failure = http::Response::builder()
            .status(500)
            .header("content-type", "application/xml")
            .body(SdkBody::from(
                "<Error><Code>InternalError</Code><Message>SECRET_ABORT_DETAIL</Message></Error>",
            ))
            .expect("build abort failure response");
        let (client, _) = test_s3_client_with_response_sequence(vec![
            multipart_copy_create_response("abort-failure-id"),
            part_failure,
            abort_failure,
        ]);
        let src = RemotePath::new("test", "source-bucket", "src.bin");
        let dst = RemotePath::new("test", "destination-bucket", "dst.bin");
        let options = multipart_copy_options(S3_MULTIPART_COPY_MIN_PART_SIZE);

        let error = client
            .multipart_copy(
                &src,
                &dst,
                &options,
                &MultipartCopyCancellation::new(),
                None,
                &|_| {},
            )
            .await
            .expect_err("part and abort failures should be reported");
        let display = error.to_string();

        assert!(matches!(error, Error::Network(_)));
        assert!(display.contains("abort-failure-id"));
        assert!(display.contains("abort: failed"));
        assert!(!display.contains("SECRET_ABORT_DETAIL"));
    }

    #[tokio::test]
    async fn delete_object_wrapper_uses_default_options_without_rustfs_header() {
        let (client, request_receiver) = test_s3_client(None);
        let path = RemotePath::new("test", "bucket", "key.txt");

        let _ = ObjectStore::delete_object(&client, &path).await;

        let request = request_receiver.expect_request();
        assert!(request.headers().get("x-rustfs-force-delete").is_none());
    }

    #[tokio::test]
    async fn delete_object_with_options_maps_missing_keys_to_not_found() {
        let response = http::Response::builder()
            .status(404)
            .header("x-amz-error-code", "NoSuchKey")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>NoSuchKey</Code>
  <Message>The specified key does not exist.</Message>
</Error>"#,
            ))
            .expect("build delete object response");
        let (client, _request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "missing.txt");

        let result = client
            .delete_object_with_options(&path, DeleteRequestOptions::default())
            .await;

        match result {
            Err(Error::NotFound(message)) => assert_eq!(message, path.to_string()),
            other => panic!("Expected NotFound for missing key, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_object_with_options_maps_other_failures_to_network() {
        let response = http::Response::builder()
            .status(500)
            .header("x-amz-error-code", "InternalError")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>InternalError</Code>
  <Message>Something went wrong.</Message>
</Error>"#,
            ))
            .expect("build delete object response");
        let (client, _request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "key.txt");

        let result = client
            .delete_object_with_options(&path, DeleteRequestOptions::default())
            .await;

        match result {
            Err(Error::Network(message)) => assert!(message.contains("InternalError")),
            other => panic!("Expected Network for delete failure, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_object_versions_page_preserves_markers_and_delete_markers() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<ListVersionsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>bucket</Name>
  <Prefix>logs/</Prefix>
  <KeyMarker></KeyMarker>
  <VersionIdMarker></VersionIdMarker>
  <NextKeyMarker>logs/c.txt</NextKeyMarker>
  <NextVersionIdMarker>v3</NextVersionIdMarker>
  <MaxKeys>25</MaxKeys>
  <IsTruncated>true</IsTruncated>
  <Version>
    <Key>logs/a.txt</Key>
    <VersionId>v1</VersionId>
    <IsLatest>true</IsLatest>
    <LastModified>2026-04-29T11:22:33.000Z</LastModified>
    <ETag>"etag-a"</ETag>
    <Size>12</Size>
    <StorageClass>STANDARD</StorageClass>
  </Version>
  <DeleteMarker>
    <Key>logs/b.txt</Key>
    <VersionId>v2</VersionId>
    <IsLatest>false</IsLatest>
    <LastModified>2026-04-28T10:20:30.000Z</LastModified>
    <Owner>
      <ID>owner</ID>
    </Owner>
  </DeleteMarker>
</ListVersionsResult>"#,
            ))
            .expect("build list object versions response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "logs/");

        let result = client
            .list_object_versions_page(&path, Some(25))
            .await
            .expect("list object versions page");

        let request = request_receiver.expect_request();
        let uri = request.uri().to_string();
        assert!(
            uri.starts_with("https://example.com/bucket/?"),
            "unexpected URI: {uri}"
        );
        assert!(
            uri.contains("versions"),
            "expected versions subresource: {uri}"
        );
        assert!(
            uri.contains("prefix=logs%2F"),
            "expected prefix query: {uri}"
        );
        assert!(
            uri.contains("max-keys=25"),
            "expected max-keys query: {uri}"
        );

        assert!(result.truncated);
        assert_eq!(result.continuation_token.as_deref(), Some("logs/c.txt"));
        assert_eq!(result.version_id_marker.as_deref(), Some("v3"));
        assert_eq!(result.items.len(), 2);

        let version = &result.items[0];
        assert_eq!(version.key, "logs/a.txt");
        assert_eq!(version.version_id, "v1");
        assert!(version.is_latest);
        assert!(!version.is_delete_marker);
        assert_eq!(version.size_bytes, Some(12));
        assert_eq!(version.etag.as_deref(), Some("etag-a"));

        let delete_marker = &result.items[1];
        assert_eq!(delete_marker.key, "logs/b.txt");
        assert_eq!(delete_marker.version_id, "v2");
        assert!(!delete_marker.is_latest);
        assert!(delete_marker.is_delete_marker);
        assert_eq!(delete_marker.size_bytes, None);
        assert_eq!(delete_marker.etag, None);
    }

    #[tokio::test]
    async fn list_object_versions_orders_same_second_entries_by_nanoseconds() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<ListVersionsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>bucket</Name>
  <Prefix>logs/a.txt</Prefix>
  <MaxKeys>25</MaxKeys>
  <IsTruncated>false</IsTruncated>
  <Version>
    <Key>logs/a.txt</Key>
    <VersionId>data-v1</VersionId>
    <IsLatest>false</IsLatest>
    <LastModified>2026-07-23T03:00:00.100Z</LastModified>
    <ETag>"etag-a"</ETag>
    <Size>12</Size>
    <StorageClass>STANDARD</StorageClass>
  </Version>
  <DeleteMarker>
    <Key>logs/a.txt</Key>
    <VersionId>marker-v2</VersionId>
    <IsLatest>true</IsLatest>
    <LastModified>2026-07-23T03:00:00.900Z</LastModified>
  </DeleteMarker>
</ListVersionsResult>"#,
            ))
            .expect("build list object versions response");
        let (client, _) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "logs/a.txt");

        let result = client
            .list_object_versions_page(&path, Some(25))
            .await
            .expect("list object versions page");

        assert_eq!(
            result
                .items
                .iter()
                .map(|version| version.version_id.as_str())
                .collect::<Vec<_>>(),
            ["marker-v2", "data-v1"]
        );
        assert!(
            result.items[0].last_modified > result.items[1].last_modified,
            "nanosecond precision must be preserved for safe undo ordering"
        );
    }

    #[tokio::test]
    async fn list_object_versions_page_maps_missing_bucket_to_not_found() {
        let response = http::Response::builder()
            .status(404)
            .header("x-amz-error-code", "NoSuchBucket")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>NoSuchBucket</Code>
  <Message>The specified bucket does not exist.</Message>
</Error>"#,
            ))
            .expect("build missing bucket response");
        let (client, _request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "missing-bucket", "");

        let result = client.list_object_versions_page(&path, Some(1000)).await;

        match result {
            Err(Error::NotFound(message)) => {
                assert_eq!(message, "Bucket not found: missing-bucket")
            }
            other => panic!("Expected NotFound for missing bucket, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_object_versions_page_maps_not_found_code_to_not_found() {
        let response = http::Response::builder()
            .status(404)
            .header("x-amz-error-code", "NotFound")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>NotFound</Code>
  <Message>The specified bucket does not exist.</Message>
</Error>"#,
            ))
            .expect("build not found bucket response");
        let (client, _request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "missing-bucket", "");

        let result = client.list_object_versions_page(&path, Some(1000)).await;

        match result {
            Err(Error::NotFound(message)) => {
                assert_eq!(message, "Bucket not found: missing-bucket")
            }
            other => panic!("Expected NotFound for NotFound list versions error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_object_versions_page_maps_other_failures_to_network() {
        let response = http::Response::builder()
            .status(500)
            .header("x-amz-error-code", "InternalError")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>InternalError</Code>
  <Message>Something went wrong.</Message>
</Error>"#,
            ))
            .expect("build internal error response");
        let (client, _request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "");

        let result = client.list_object_versions_page(&path, Some(1000)).await;

        match result {
            Err(Error::Network(message)) => assert!(message.contains("InternalError")),
            other => panic!("Expected Network for list versions failure, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_object_versions_page_maps_permission_denied_to_auth() {
        let response = http::Response::builder()
            .status(403)
            .header("x-amz-error-code", "AccessDenied")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error><Code>AccessDenied</Code><Message>Access denied.</Message></Error>"#,
            ))
            .expect("build access denied response");
        let (client, _request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "");

        let result = client.list_object_versions_page(&path, Some(1000)).await;

        assert!(matches!(result, Err(Error::Auth(_))));
    }

    #[tokio::test]
    async fn list_objects_maps_permission_denied_to_auth() {
        let response = http::Response::builder()
            .status(403)
            .header("x-amz-error-code", "AccessDenied")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error><Code>AccessDenied</Code><Message>Access denied.</Message></Error>"#,
            ))
            .expect("build access denied response");
        let (client, _request_receiver) = test_s3_client(Some(response));
        let path = RemotePath::new("test", "bucket", "");

        let result = client
            .list_objects(
                &path,
                ListOptions {
                    recursive: true,
                    ..Default::default()
                },
            )
            .await;

        assert!(matches!(result, Err(Error::Auth(_))));
    }

    #[tokio::test]
    async fn list_buckets_preserves_service_error_code() {
        let response = http::Response::builder()
            .status(403)
            .header("x-amz-error-code", "InvalidAccessKeyId")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>InvalidAccessKeyId</Code>
  <Message>The AWS access key Id you provided does not exist in our records.</Message>
</Error>"#,
            ))
            .expect("build list buckets response");
        let (client, _request_receiver) = test_s3_client(Some(response));

        let result = client.list_buckets().await;

        match result {
            Err(Error::Auth(message)) => assert!(message.contains("InvalidAccessKeyId")),
            other => panic!("Expected Auth for list buckets failure, got: {other:?}"),
        }
    }

    #[test]
    fn truncated_listing_requires_a_new_continuation_token() {
        assert!(validate_continuation_token(false, None, None).is_ok());
        assert!(validate_continuation_token(true, None, Some("next")).is_ok());
        assert!(validate_continuation_token(true, Some("current"), None).is_err());
        assert!(validate_continuation_token(true, Some("current"), Some("current")).is_err());
    }

    #[test]
    fn alias_retry_and_timeout_configs_are_validated() {
        let retry = rc_core::alias::RetryConfig {
            max_attempts: 0,
            ..Default::default()
        };
        let timeout = rc_core::alias::TimeoutConfig {
            connect_ms: 0,
            ..Default::default()
        };

        assert!(sdk_retry_config(&retry).is_err());
        assert!(sdk_timeout_config(&timeout).is_err());
        assert!(sdk_retry_config(&Default::default()).is_ok());
        assert!(sdk_timeout_config(&Default::default()).is_ok());
    }

    #[tokio::test]
    async fn create_bucket_maps_access_denial_to_auth() {
        let response = http::Response::builder()
            .status(403)
            .header("x-amz-error-code", "InvalidAccessKeyId")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>InvalidAccessKeyId</Code>
  <Message>The AWS access key Id you provided does not exist in our records.</Message>
</Error>"#,
            ))
            .expect("build create bucket response");
        let (client, _request_receiver) = test_s3_client(Some(response));

        let result = client.create_bucket("bucket").await;

        match result {
            Err(Error::Auth(message)) => assert!(message.contains("InvalidAccessKeyId")),
            other => panic!("Expected Auth for create bucket failure, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_bucket_with_options_sends_region_and_object_lock() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::empty())
            .expect("build create bucket response");
        let (client, request_receiver) = test_s3_client(Some(response));
        let options = CreateBucketOptions::for_cli(Some("eu-west-1".to_string()), false, true)
            .expect("valid create options");

        ObjectStore::create_bucket_with_options(&client, "locked-bucket", &options)
            .await
            .expect("create bucket with options");

        let request = request_receiver.expect_request();
        assert_eq!(
            request.headers().get("x-amz-bucket-object-lock-enabled"),
            Some("true")
        );
        let body = request.body().bytes().expect("request body bytes");
        let body = std::str::from_utf8(body).expect("request body is utf8");
        assert!(body.contains("<LocationConstraint>eu-west-1</LocationConstraint>"));
    }

    #[tokio::test]
    async fn create_bucket_with_default_options_omits_region_and_object_lock() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::empty())
            .expect("build create bucket response");
        let (client, request_receiver) = test_s3_client(Some(response));

        ObjectStore::create_bucket_with_options(
            &client,
            "plain-bucket",
            &CreateBucketOptions::default(),
        )
        .await
        .expect("create bucket without options");

        let request = request_receiver.expect_request();
        assert!(
            request
                .headers()
                .get("x-amz-bucket-object-lock-enabled")
                .is_none()
        );
        assert!(request.body().bytes().unwrap_or_default().is_empty());
    }

    #[tokio::test]
    async fn create_bucket_with_invalid_options_makes_no_request() {
        let (client, request_receiver) = test_s3_client(None);
        let invalid = CreateBucketOptions {
            region: None,
            versioning_enabled: false,
            object_lock_enabled: true,
        };

        let result = ObjectStore::create_bucket_with_options(&client, "bucket", &invalid).await;

        assert!(matches!(result, Err(Error::InvalidPath(_))));
        request_receiver.expect_no_request();
    }

    #[tokio::test]
    async fn get_bucket_location_returns_the_service_reported_constraint() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<LocationConstraint xmlns="http://s3.amazonaws.com/doc/2006-03-01/">eu-west-1</LocationConstraint>"#,
            ))
            .expect("build bucket location response");
        let (client, request_receiver) = test_s3_client(Some(response));

        let location = ObjectStore::get_bucket_location(&client, "bucket")
            .await
            .expect("read bucket location");

        assert_eq!(location.as_deref(), Some("eu-west-1"));
        let request = request_receiver.expect_request();
        assert_eq!(request.method(), http::Method::GET);
        assert!(request.uri().contains("?location"));
    }

    #[tokio::test]
    async fn delete_bucket_maps_bucket_not_empty_to_conflict() {
        let response = http::Response::builder()
            .status(409)
            .header("x-amz-error-code", "BucketNotEmpty")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>BucketNotEmpty</Code>
  <Message>The bucket you tried to delete is not empty.</Message>
</Error>"#,
            ))
            .expect("build delete bucket response");
        let (client, _request_receiver) = test_s3_client(Some(response));

        let result = client.delete_bucket("bucket").await;

        match result {
            Err(Error::Conflict(message)) => assert!(message.contains("BucketNotEmpty")),
            other => panic!("Expected Conflict for non-empty bucket, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_bucket_maps_missing_bucket_to_not_found() {
        let response = http::Response::builder()
            .status(404)
            .header("x-amz-error-code", "NoSuchBucket")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>NoSuchBucket</Code>
  <Message>The specified bucket does not exist.</Message>
</Error>"#,
            ))
            .expect("build missing bucket response");
        let (client, _request_receiver) = test_s3_client(Some(response));

        let result = client.delete_bucket("missing-bucket").await;

        match result {
            Err(Error::NotFound(message)) => {
                assert_eq!(message, "Bucket not found: missing-bucket")
            }
            other => panic!("Expected NotFound for missing bucket, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_bucket_maps_not_found_code_to_not_found() {
        let response = http::Response::builder()
            .status(404)
            .header("x-amz-error-code", "NotFound")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>NotFound</Code>
  <Message>The specified bucket does not exist.</Message>
</Error>"#,
            ))
            .expect("build not found bucket response");
        let (client, _request_receiver) = test_s3_client(Some(response));

        let result = client.delete_bucket("missing-bucket").await;

        match result {
            Err(Error::NotFound(message)) => {
                assert_eq!(message, "Bucket not found: missing-bucket")
            }
            other => panic!("Expected NotFound for NotFound delete bucket error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_bucket_maps_other_failures_to_network() {
        let response = http::Response::builder()
            .status(500)
            .header("x-amz-error-code", "InternalError")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>InternalError</Code>
  <Message>Something went wrong.</Message>
</Error>"#,
            ))
            .expect("build delete bucket response");
        let (client, _request_receiver) = test_s3_client(Some(response));

        let result = client.delete_bucket("bucket").await;

        match result {
            Err(Error::Network(message)) => assert!(message.contains("InternalError")),
            other => panic!("Expected Network for delete bucket failure, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_bucket_keeps_authentication_failures_distinct() {
        let response = http::Response::builder()
            .status(403)
            .header("x-amz-error-code", "AccessDenied")
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>AccessDenied</Code>
  <Message>Access denied.</Message>
</Error>"#,
            ))
            .expect("build delete bucket auth response");
        let (client, _request_receiver) = test_s3_client(Some(response));

        let result = client.delete_bucket("bucket").await;

        match result {
            Err(Error::Auth(message)) => assert!(message.contains("AccessDenied")),
            other => panic!("Expected Auth for delete bucket denial, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_objects_with_force_delete_sets_rustfs_header() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<DeleteResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/" />"#,
            ))
            .expect("build delete objects response");
        let (client, request_receiver) = test_s3_client(Some(response));

        let _ = client
            .delete_objects_with_options(
                "bucket",
                vec!["key.txt".to_string()],
                DeleteRequestOptions {
                    force_delete: true,
                    ..Default::default()
                },
            )
            .await;

        let request = request_receiver.expect_request();
        assert_eq!(request.headers().get("x-rustfs-force-delete"), Some("true"));
    }

    #[tokio::test]
    async fn delete_objects_with_bypass_sets_governance_header() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<DeleteResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/" />"#,
            ))
            .expect("build delete objects response");
        let (client, request_receiver) = test_s3_client(Some(response));

        let _ = client
            .delete_objects_with_options(
                "bucket",
                vec!["key.txt".to_string()],
                DeleteRequestOptions {
                    bypass_governance: true,
                    ..Default::default()
                },
            )
            .await;

        let request = request_receiver.expect_request();
        assert_eq!(
            request.headers().get("x-amz-bypass-governance-retention"),
            Some("true")
        );
    }

    #[tokio::test]
    async fn delete_objects_without_force_delete_omits_rustfs_header() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<DeleteResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/" />"#,
            ))
            .expect("build delete objects response");
        let (client, request_receiver) = test_s3_client(Some(response));

        let _ = client
            .delete_objects_with_options(
                "bucket",
                vec!["key.txt".to_string()],
                DeleteRequestOptions::default(),
            )
            .await;

        let request = request_receiver.expect_request();
        assert!(request.headers().get("x-rustfs-force-delete").is_none());
    }

    #[tokio::test]
    async fn delete_object_versions_preserves_versions_markers_and_bypass() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<DeleteResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Deleted><Key>key.txt</Key><VersionId>v1</VersionId></Deleted>
  <Deleted><Key>key.txt</Key><VersionId>marker-v2</VersionId><DeleteMarker>true</DeleteMarker><DeleteMarkerVersionId>marker-v2</DeleteMarkerVersionId></Deleted>
</DeleteResult>"#,
            ))
            .expect("build version delete response");
        let (client, request_receiver) = test_s3_client(Some(response));

        let result = client
            .delete_object_versions_with_options(
                "bucket",
                vec![
                    ObjectVersionIdentifier {
                        key: "key.txt".to_string(),
                        version_id: Some("v1".to_string()),
                        is_delete_marker: false,
                    },
                    ObjectVersionIdentifier {
                        key: "key.txt".to_string(),
                        version_id: Some("marker-v2".to_string()),
                        is_delete_marker: true,
                    },
                ],
                DeleteRequestOptions {
                    bypass_governance: true,
                    ..Default::default()
                },
            )
            .await
            .expect("delete exact versions");

        let request = request_receiver.expect_request();
        assert_eq!(
            request.headers().get("x-amz-bypass-governance-retention"),
            Some("true")
        );
        let body = request.body().bytes().expect("request body bytes");
        let body = std::str::from_utf8(body).expect("request body is utf8");
        assert!(body.contains("<VersionId>v1</VersionId>"));
        assert!(body.contains("<VersionId>marker-v2</VersionId>"));
        assert_eq!(result.deleted.len(), 2);
        assert!(result.deleted[1].is_delete_marker);
        assert!(result.failures.is_empty());
    }

    #[tokio::test]
    async fn delete_objects_wrapper_uses_default_options_without_rustfs_header() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<DeleteResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/" />"#,
            ))
            .expect("build delete objects response");
        let (client, request_receiver) = test_s3_client(Some(response));

        let _ = ObjectStore::delete_objects(&client, "bucket", vec!["key.txt".to_string()]).await;

        let request = request_receiver.expect_request();
        assert!(request.headers().get("x-rustfs-force-delete").is_none());
    }

    #[tokio::test]
    async fn delete_objects_with_empty_keys_skips_http_request() {
        let (client, request_receiver) = test_s3_client(None);

        let deleted = client
            .delete_objects_with_options("bucket", Vec::new(), DeleteRequestOptions::default())
            .await
            .expect("empty delete should succeed");

        assert!(deleted.is_empty());
        request_receiver.expect_no_request();
    }

    #[tokio::test]
    async fn delete_objects_with_partial_errors_returns_deleted_keys() {
        let response = http::Response::builder()
            .status(200)
            .body(SdkBody::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<DeleteResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Deleted>
    <Key>kept.txt</Key>
  </Deleted>
  <Error>
    <Key>failed.txt</Key>
    <Code>AccessDenied</Code>
    <Message>Access Denied</Message>
  </Error>
</DeleteResult>"#,
            ))
            .expect("build partial delete response");
        let (client, request_receiver) = test_s3_client(Some(response));

        let deleted = client
            .delete_objects_with_options(
                "bucket",
                vec!["kept.txt".to_string(), "failed.txt".to_string()],
                DeleteRequestOptions::default(),
            )
            .await
            .expect("partial delete should still return deleted keys");

        let request = request_receiver.expect_request();
        assert_eq!(request.uri(), "https://example.com/bucket/?delete");
        assert_eq!(deleted, vec!["kept.txt".to_string()]);
    }

    #[tokio::test]
    async fn read_next_part_fills_buffer_until_eof() {
        use tokio::io::AsyncWriteExt;

        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let file_path = temp_dir.path().join("payload.bin");
        let mut writer = tokio::fs::File::create(&file_path)
            .await
            .expect("create temp file");
        writer
            .write_all(b"abcdefghij")
            .await
            .expect("write temp file");
        writer.flush().await.expect("flush temp file");
        drop(writer);

        let mut reader = tokio::fs::File::open(&file_path)
            .await
            .expect("open temp file");
        let mut buffer = vec![0u8; 8];

        let first = S3Client::read_next_part(&mut reader, &file_path, &mut buffer)
            .await
            .expect("first read");
        assert_eq!(first, 8);
        assert_eq!(&buffer[..first], b"abcdefgh");

        let second = S3Client::read_next_part(&mut reader, &file_path, &mut buffer)
            .await
            .expect("second read");
        assert_eq!(second, 2);
        assert_eq!(&buffer[..second], b"ij");

        let third = S3Client::read_next_part(&mut reader, &file_path, &mut buffer)
            .await
            .expect("third read");
        assert_eq!(third, 0);
    }
}
