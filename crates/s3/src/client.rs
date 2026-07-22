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
use bytes::Bytes;
use futures::TryStreamExt as _;
use http_body::Frame;
use http_body_util::StreamBody;
use jiff::Timestamp;
use quick_xml::de::from_str as from_xml_str;
pub use rc_core::DeleteRequestOptions;
use rc_core::{
    Alias, BucketEncryption, BucketNotification, Capabilities, CorsRule, DeleteObjectFailure,
    DeleteObjectsResult, DeletedObject, Error, LifecycleRule, ListObjectVersionsOptions,
    ListOptions, ListResult, NotificationTarget, ObjectEncryptionRequest, ObjectInfo,
    ObjectReadOptions, ObjectStore, ObjectVersion, ObjectVersionIdentifier,
    ObjectVersionListResult, RemotePath, ReplicationConfiguration, ReplicationResyncStartOptions,
    ReplicationResyncStartResult, ReplicationResyncState, ReplicationResyncStatus,
    ReplicationResyncTargetStatus, RequestHeader, Result, SelectOptions, global_request_headers,
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

/// Keep single-part uploads small to avoid backend incompatibilities with
/// streaming aws-chunked payloads.
const SINGLE_PUT_OBJECT_MAX_SIZE: u64 = crate::multipart::DEFAULT_PART_SIZE;
const S3_SERVICE_NAME: &str = "s3";
const S3_REPLICATION_XML_NAMESPACE: &str = "http://s3.amazonaws.com/doc/2006-03-01/";
const RUSTFS_FORCE_DELETE_HEADER: &str = "x-rustfs-force-delete";
const REPLICATION_EXTENSION_BODY_LIMIT: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy)]
enum ObjectWritePrecondition<'a> {
    None,
    IfAbsent,
    IfMatch(&'a str),
}

#[derive(Debug, Clone, Copy)]
struct PathUploadOptions<'a> {
    content_type: Option<&'a str>,
    encryption: Option<&'a ObjectEncryptionRequest>,
    precondition: ObjectWritePrecondition<'a>,
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
                    .and_then(|dt| Timestamp::from_second(dt.secs()).ok()),
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
                    .and_then(|dt| Timestamp::from_second(dt.secs()).ok()),
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
        mut on_progress: impl FnMut(u64, Option<u64>) + Send,
    ) -> Result<u64> {
        let response = self
            .inner
            .get_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .send()
            .await
            .map_err(|error| {
                let message = error.to_string();
                if message.contains("NotFound") || message.contains("NoSuchKey") {
                    Error::NotFound(path.to_string())
                } else {
                    Error::Network(message)
                }
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
                return Err(Error::Network(error.to_string()));
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
        let body = aws_sdk_s3::primitives::ByteStream::from(data);

        let mut request = apply_object_encryption_to_put_request(
            self.inner
                .put_object()
                .bucket(&path.bucket)
                .key(&path.key)
                .body(body),
            options.encryption,
        );

        if let Some(ct) = options.content_type {
            request = request.content_type(ct);
        }
        request = match options.precondition {
            ObjectWritePrecondition::None => request,
            ObjectWritePrecondition::IfAbsent => request.if_none_match("*"),
            ObjectWritePrecondition::IfMatch(etag) => request.if_match(etag),
        };

        let response = request.send().await.map_err(|error| {
            if !matches!(options.precondition, ObjectWritePrecondition::None)
                && let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &error
                && matches!(service_error.raw().status().as_u16(), 409 | 412)
            {
                Error::Conflict(format!("Object changed before upload: {path}"))
            } else {
                Error::Network(Self::format_sdk_error(&error))
            }
        })?;

        let mut info = ObjectInfo::file(&path.key, file_size as i64);
        if let Some(etag) = response.e_tag() {
            info.etag = Some(etag.trim_matches('"').to_string());
        }
        info.version_id = response.version_id().map(ToString::to_string);
        info.last_modified = Some(jiff::Timestamp::now());

        Ok(info)
    }

    async fn abort_multipart_upload_best_effort(&self, path: &RemotePath, upload_id: &str) {
        let _ = self
            .inner
            .abort_multipart_upload()
            .bucket(&path.bucket)
            .key(&path.key)
            .upload_id(upload_id)
            .send()
            .await;
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

        let config = crate::multipart::MultipartConfig::default();
        let part_size = config.calculate_part_size(file_size);
        let part_buffer_size = usize::try_from(part_size)
            .map_err(|_| Error::General(format!("invalid part size: {part_size}")))?;
        let mut file = tokio::fs::File::open(file_path)
            .await
            .map_err(|e| Error::General(format!("open file '{}': {e}", file_path.display())))?;
        let mut chunk = vec![0u8; part_buffer_size];

        tracing::debug!(file_size, part_size, "Starting multipart upload");

        let mut create_request = self
            .inner
            .create_multipart_upload()
            .bucket(&path.bucket)
            .key(&path.key);

        create_request = match options.encryption {
            Some(ObjectEncryptionRequest::SseS3) => create_request
                .server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::Aes256),
            Some(ObjectEncryptionRequest::SseKms { key_id }) => create_request
                .server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::AwsKms)
                .ssekms_key_id(key_id),
            None => create_request,
        };

        if let Some(ct) = options.content_type {
            create_request = create_request.content_type(ct);
        }

        let create_response = create_request
            .send()
            .await
            .map_err(|e| Error::Network(format!("create multipart upload: {e}")))?;

        let upload_id = create_response
            .upload_id()
            .ok_or_else(|| Error::General("missing upload id from multipart upload".to_string()))?
            .to_string();

        tracing::debug!(upload_id = %upload_id, "Multipart upload initiated");

        let mut completed_parts = Vec::new();
        let mut part_number: i32 = 1;
        let mut bytes_uploaded: u64 = 0;

        loop {
            let bytes_read = match Self::read_next_part(&mut file, file_path, &mut chunk).await {
                Ok(bytes_read) => bytes_read,
                Err(error) => {
                    self.abort_multipart_upload_best_effort(path, &upload_id)
                        .await;
                    return Err(error);
                }
            };
            // A conditional zero-byte write still needs a multipart completion
            // request, because RustFS evaluates destination preconditions there.
            // S3 permits the final part to be smaller than the minimum part size.
            if bytes_read == 0 && !(file_size == 0 && part_number == 1) {
                break;
            }

            tracing::debug!(part_number, bytes_read, "Uploading part");

            let body = aws_sdk_s3::primitives::ByteStream::from(chunk[..bytes_read].to_vec());
            let upload_part_result = self
                .inner
                .upload_part()
                .bucket(&path.bucket)
                .key(&path.key)
                .upload_id(&upload_id)
                .part_number(part_number)
                .body(body)
                .send()
                .await;

            let upload_part_response = match upload_part_result {
                Ok(response) => response,
                Err(e) => {
                    tracing::debug!(
                        upload_id = %upload_id,
                        part_number,
                        "Aborting multipart upload due to error"
                    );
                    self.abort_multipart_upload_best_effort(path, &upload_id)
                        .await;
                    return Err(Error::Network(format!(
                        "upload multipart part {part_number}: {e}"
                    )));
                }
            };

            let etag = match upload_part_response.e_tag() {
                Some(value) => value.trim_matches('"').to_string(),
                None => {
                    self.abort_multipart_upload_best_effort(path, &upload_id)
                        .await;
                    return Err(Error::General(format!(
                        "missing ETag for multipart part {part_number}"
                    )));
                }
            };

            completed_parts.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(etag)
                    .build(),
            );

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
                self.abort_multipart_upload_best_effort(path, &upload_id)
                    .await;
                if !matches!(options.precondition, ObjectWritePrecondition::None)
                    && let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &error
                    && matches!(service_error.raw().status().as_u16(), 409 | 412)
                {
                    return Err(Error::Conflict(format!(
                        "Object changed before upload: {path}"
                    )));
                }
                return Err(Error::Network(format!(
                    "complete multipart upload: {}",
                    Self::format_sdk_error(&error)
                )));
            }
        };

        tracing::debug!("Multipart upload completed");

        let mut info = ObjectInfo::file(&path.key, file_size as i64);
        if let Some(etag) = complete_response.e_tag() {
            info.etag = Some(etag.trim_matches('"').to_string());
        }
        info.version_id = complete_response.version_id().map(ToString::to_string);
        info.last_modified = Some(jiff::Timestamp::now());

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
        self.put_object_from_path_with_condition(
            path,
            file_path,
            content_type,
            encryption,
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
        self.put_object_from_path_with_condition(
            path,
            file_path,
            content_type,
            encryption,
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
        self.put_object_from_path_with_condition(
            path,
            file_path,
            content_type,
            encryption,
            ObjectWritePrecondition::IfMatch(etag),
            on_progress,
        )
        .await
    }

    async fn put_object_from_path_with_condition(
        &self,
        path: &RemotePath,
        file_path: &std::path::Path,
        content_type: Option<&str>,
        encryption: Option<&ObjectEncryptionRequest>,
        precondition: ObjectWritePrecondition<'_>,
        on_progress: impl Fn(u64) + Send,
    ) -> Result<ObjectInfo> {
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
            content_type,
            encryption,
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
        self.inner
            .create_bucket()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| Error::Network(Self::format_sdk_error(&e)))?;

        Ok(())
    }

    async fn delete_bucket(&self, bucket: &str) -> Result<()> {
        self.inner
            .delete_bucket()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| {
                let err_str = Self::format_sdk_error(&e);
                if err_str.contains("NotFound") || err_str.contains("NoSuchBucket") {
                    Error::NotFound(format!("Bucket not found: {bucket}"))
                } else if err_str.contains("BucketNotEmpty") {
                    Error::Conflict(err_str)
                } else {
                    Error::Network(err_str)
                }
            })?;

        Ok(())
    }

    async fn capabilities(&self) -> Result<Capabilities> {
        // Best-effort hints for common S3-compatible backends. `select` is not inferred here
        // because `rc sql` determines support from the real request result.
        Ok(Capabilities {
            versioning: true,
            object_lock: false,
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

    async fn write_object_to_with_options(
        &self,
        path: &RemotePath,
        options: &ObjectReadOptions,
        writer: &mut (dyn AsyncWrite + Send + Unpin),
        max_bytes: Option<u64>,
    ) -> Result<u64> {
        S3Client::write_object_to_with_options(self, path, options, writer, max_bytes).await
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
        // Build copy source: bucket/key
        let copy_source = format!("{}/{}", src.bucket, src.key);

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
        .map_err(|e| {
            let err_str = e.to_string();
            if err_str.contains("NotFound") || err_str.contains("NoSuchKey") {
                Error::NotFound(src.to_string())
            } else {
                Error::Network(err_str)
            }
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
            let status = match rule.status {
                rc_core::LifecycleRuleStatus::Enabled => ExpirationStatus::Enabled,
                rc_core::LifecycleRuleStatus::Disabled => ExpirationStatus::Disabled,
            };

            let filter = build_lifecycle_rule_filter(rule.prefix.as_deref(), rule.tags.as_ref())?;

            let expiration = rule.expiration.map(|exp| {
                let mut builder = SdkExpiration::builder();
                if let Some(days) = exp.days {
                    builder = builder.days(days);
                }
                if let Some(ref date_str) = exp.date
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
                builder.build()
            });

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
        let url = self.replication_extension_url(bucket, "replication-check", &[])?;
        let body = self
            .signed_replication_extension_request(Method::GET, url)
            .await?;
        if !body.is_empty() {
            return Err(Error::General(
                "Malformed replication check response: expected an empty body".to_string(),
            ));
        }
        Ok(())
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
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

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

        client
            .put_object_multipart_from_path(
                &path,
                source.path(),
                4,
                PathUploadOptions {
                    content_type: Some("text/plain"),
                    encryption: None,
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

            let result = client
                .put_object_multipart_from_path(
                    &path,
                    source.path(),
                    4,
                    PathUploadOptions {
                        content_type: None,
                        encryption: None,
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

        let result = client
            .put_object_multipart_from_path(
                &path,
                source.path(),
                4,
                PathUploadOptions {
                    content_type: None,
                    encryption: None,
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

        client
            .check_bucket_replication("source-bucket")
            .await
            .expect("replication check should succeed");

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
    async fn replication_check_rejects_nonempty_success_body() {
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
    async fn create_bucket_preserves_service_error_code() {
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
            Err(Error::Network(message)) => assert!(message.contains("InvalidAccessKeyId")),
            other => panic!("Expected Network for create bucket failure, got: {other:?}"),
        }
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
