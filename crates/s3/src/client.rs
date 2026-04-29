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
use aws_smithy_runtime_api::client::http::{
    HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpConnector,
};
use aws_smithy_runtime_api::client::orchestrator::HttpRequest;
use aws_smithy_runtime_api::client::result::ConnectorError;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_runtime_api::http::{Response, StatusCode};
use aws_smithy_types::body::SdkBody;
use bytes::Bytes;
use jiff::Timestamp;
use quick_xml::de::from_str as from_xml_str;
use rc_core::{
    Alias, BucketNotification, Capabilities, CorsRule, Error, LifecycleRule, ListOptions,
    ListResult, NotificationTarget, ObjectInfo, ObjectStore, ObjectVersion,
    ObjectVersionListResult, RemotePath, ReplicationConfiguration, Result, SelectOptions,
};
use reqwest::Method;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;

/// Keep single-part uploads small to avoid backend incompatibilities with
/// streaming aws-chunked payloads.
const SINGLE_PUT_OBJECT_MAX_SIZE: u64 = crate::multipart::DEFAULT_PART_SIZE;
const S3_SERVICE_NAME: &str = "s3";
const S3_REPLICATION_XML_NAMESPACE: &str = "http://s3.amazonaws.com/doc/2006-03-01/";
const RUSTFS_FORCE_DELETE_HEADER: &str = "x-rustfs-force-delete";

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
    async fn new(insecure: bool, ca_bundle: Option<&str>) -> Result<Self> {
        let client = build_reqwest_client(insecure, ca_bundle).await?;
        Ok(Self { client })
    }
}

async fn build_reqwest_client(insecure: bool, ca_bundle: Option<&str>) -> Result<reqwest::Client> {
    // NOTE: When `insecure = true`, `danger_accept_invalid_certs` disables all TLS
    // certificate verification. Any CA bundle provided will still be added to the
    // trust store but is rendered ineffective for this connection.
    let mut builder = reqwest::Client::builder().danger_accept_invalid_certs(insecure);

    if let Some(bundle_path) = ca_bundle {
        // Use tokio::fs::read to avoid blocking the async runtime thread.
        let pem = tokio::fs::read(bundle_path).await.map_err(|e| {
            Error::Network(format!("Failed to read CA bundle '{bundle_path}': {e}"))
        })?;
        let cert = reqwest::Certificate::from_pem(&pem)
            .map_err(|e| Error::Network(format!("Invalid CA bundle '{bundle_path}': {e}")))?;
        builder = builder.add_root_certificate(cert);
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
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let client = self.client.clone();
        HttpConnectorFuture::new(async move {
            // Extract request parts before consuming the request
            let uri = request.uri().to_string();
            let method_str = request.method().to_string();
            let headers = request.headers().clone();

            // Try to get the body as buffered in-memory bytes.
            // For streaming bodies (e.g., large file uploads), bytes() returns None and we
            // return a clear error rather than silently sending an empty body, which would
            // cause signature mismatches or server-side failures.
            let body_bytes = match request.body().bytes() {
                Some(b) => Bytes::copy_from_slice(b),
                None => {
                    return Err(ConnectorError::user(
                        "Streaming request bodies are not supported in insecure/ca_bundle TLS mode; \
                         use in-memory data for uploads with this connector"
                            .into(),
                    ));
                }
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
            *req.body_mut() = Some(reqwest::Body::from(body_bytes));

            // Execute
            let resp = client
                .execute(req)
                .await
                .map_err(|e| ConnectorError::io(Box::new(e)))?;

            // Convert response
            let status = StatusCode::try_from(resp.status().as_u16())
                .map_err(|e| ConnectorError::other(Box::new(e), None))?;
            let resp_headers = resp.headers().clone();
            let body = resp
                .bytes()
                .await
                .map_err(|e| ConnectorError::io(Box::new(e)))?;

            let mut sdk_response = Response::new(status, SdkBody::from(body));
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
    xml_http_client: reqwest::Client,
    alias: Alias,
}

/// Request-level options for delete operations.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeleteRequestOptions {
    /// Ask RustFS to permanently delete data instead of creating delete markers.
    pub force_delete: bool,
}

impl S3Client {
    /// Create a new S3 client from an alias configuration
    pub async fn new(alias: Alias) -> Result<Self> {
        let endpoint = alias.endpoint.clone();
        let region = alias.region.clone();
        let access_key = alias.access_key.clone();
        let secret_key = alias.secret_key.clone();

        // Build credentials provider
        let credentials = aws_credential_types::Credentials::new(
            access_key,
            secret_key,
            None, // session token
            None, // expiry
            "rc-static-credentials",
        );

        // Build SDK config loader
        let mut config_loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .credentials_provider(credentials)
            .region(aws_config::Region::new(region))
            .endpoint_url(&endpoint);

        // When insecure mode is enabled or a custom CA bundle is provided, use the reqwest
        // connector which supports danger_accept_invalid_certs and custom root certificates.
        if alias.insecure || alias.ca_bundle.is_some() {
            let connector =
                ReqwestConnector::new(alias.insecure, alias.ca_bundle.as_deref()).await?;
            config_loader = config_loader.http_client(connector);
        }

        let xml_http_client =
            build_reqwest_client(alias.insecure, alias.ca_bundle.as_deref()).await?;
        let config = config_loader.load().await;

        // Build S3 client with path-style addressing for compatibility
        let s3_config = aws_sdk_s3::config::Builder::from(&config)
            .force_path_style(force_path_style_for_alias(&alias))
            // Improve compatibility with S3-compatible backends by only sending request
            // checksums when the operation explicitly requires them.
            .request_checksum_calculation(
                aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
            )
            .response_checksum_validation(
                aws_sdk_s3::config::ResponseChecksumValidation::WhenRequired,
            )
            .build();

        let client = aws_sdk_s3::Client::from_conf(s3_config);

        Ok(Self {
            inner: client,
            xml_http_client,
            alias,
        })
    }

    /// Get the underlying aws-sdk-s3 client
    pub fn inner(&self) -> &aws_sdk_s3::Client {
        &self.inner
    }

    /// List a single page of object versions and return pagination metadata.
    pub async fn list_object_versions_page(
        &self,
        path: &RemotePath,
        max_keys: Option<i32>,
    ) -> Result<ObjectVersionListResult> {
        let mut builder = self.inner.list_object_versions().bucket(&path.bucket);

        if !path.key.is_empty() {
            builder = builder.prefix(&path.key);
        }

        if let Some(max) = max_keys {
            builder = builder.max_keys(max);
        }

        let response = builder.send().await.map_err(|e| {
            let err_str = e.to_string();
            if err_str.contains("NotFound") || err_str.contains("NoSuchBucket") {
                Error::NotFound(format!("Bucket not found: {}", path.bucket))
            } else {
                Error::General(format!("list_object_versions: {e}"))
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
        mut on_progress: impl FnMut(u64, Option<u64>) + Send,
    ) -> Result<Vec<u8>> {
        let response = self
            .inner
            .get_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .send()
            .await
            .map_err(|e| {
                let err_str = e.to_string();
                if err_str.contains("NotFound") || err_str.contains("NoSuchKey") {
                    Error::NotFound(path.to_string())
                } else {
                    Error::Network(err_str)
                }
            })?;

        let content_length = response
            .content_length()
            .and_then(|length| u64::try_from(length).ok());
        let mut data = Vec::with_capacity(
            content_length
                .and_then(|length| usize::try_from(length).ok())
                .unwrap_or_default(),
        );
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

    /// Delete an object with RustFS-specific request options.
    pub async fn delete_object_with_options(
        &self,
        path: &RemotePath,
        options: DeleteRequestOptions,
    ) -> Result<()> {
        let mut request = self
            .inner
            .delete_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .customize();

        if options.force_delete {
            request = request.mutate_request(|request| {
                request
                    .headers_mut()
                    .insert(RUSTFS_FORCE_DELETE_HEADER, "true");
            });
        }

        request.send().await.map_err(|e| {
            let err_str = Self::format_sdk_error(&e);
            let is_missing_key = if let aws_sdk_s3::error::SdkError::ServiceError(service_err) = &e
            {
                let code = service_err.err().code().or_else(|| {
                    service_err
                        .raw()
                        .headers()
                        .get("x-amz-error-code")
                        .and_then(|value| std::str::from_utf8(value.as_bytes()).ok())
                });
                matches!(code, Some("NoSuchKey") | Some("NotFound"))
                    || service_err.raw().status().as_u16() == 404
            } else {
                err_str.contains("NotFound") || err_str.contains("NoSuchKey")
            };

            if is_missing_key {
                Error::NotFound(path.to_string())
            } else {
                Error::Network(err_str)
            }
        })?;

        Ok(())
    }

    /// Delete multiple objects with RustFS-specific request options.
    pub async fn delete_objects_with_options(
        &self,
        bucket: &str,
        keys: Vec<String>,
        options: DeleteRequestOptions,
    ) -> Result<Vec<String>> {
        use aws_sdk_s3::types::{Delete, ObjectIdentifier};

        if keys.is_empty() {
            return Ok(vec![]);
        }

        let objects: Vec<ObjectIdentifier> =
            keys.iter()
                .map(|key| {
                    ObjectIdentifier::builder().key(key).build().map_err(|e| {
                        Error::General(format!("invalid delete object identifier: {e}"))
                    })
                })
                .collect::<Result<Vec<_>>>()?;

        let delete = Delete::builder()
            .set_objects(Some(objects))
            .build()
            .map_err(|e| Error::General(e.to_string()))?;

        let mut request = self
            .inner
            .delete_objects()
            .bucket(bucket)
            .delete(delete)
            .customize();

        if options.force_delete {
            request = request.mutate_request(|request| {
                request
                    .headers_mut()
                    .insert(RUSTFS_FORCE_DELETE_HEADER, "true");
            });
        }

        let response = request
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;

        let deleted: Vec<String> = response
            .deleted()
            .iter()
            .filter_map(|d| d.key().map(|k| k.to_string()))
            .collect();

        if !response.errors().is_empty() {
            let error_keys: Vec<String> = response
                .errors()
                .iter()
                .filter_map(|e| e.key().map(|k| k.to_string()))
                .collect();
            tracing::warn!("Failed to delete some objects: {:?}", error_keys);
        }

        Ok(deleted)
    }

    /// Format AWS SDK error into a detailed error message
    fn format_sdk_error<E: std::fmt::Display>(error: &aws_sdk_s3::error::SdkError<E>) -> String {
        match error {
            aws_sdk_s3::error::SdkError::ServiceError(service_err) => {
                let err = service_err.err();
                let meta = service_err.raw();
                let mut msg = format!("Service error: {}", err);
                // Try to extract additional error information from headers
                if let Some(code) = meta.headers().get("x-amz-error-code")
                    && let Ok(code_str) = std::str::from_utf8(code.as_bytes())
                {
                    msg.push_str(&format!(" (code: {})", code_str));
                }
                msg
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
        content_type: Option<&str>,
        file_size: u64,
    ) -> Result<ObjectInfo> {
        let data = tokio::fs::read(file_path)
            .await
            .map_err(|e| Error::General(format!("read file '{}': {e}", file_path.display())))?;
        let body = aws_sdk_s3::primitives::ByteStream::from(data);

        let mut request = self
            .inner
            .put_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .body(body);

        if let Some(ct) = content_type {
            request = request.content_type(ct);
        }

        let response = request
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;

        let mut info = ObjectInfo::file(&path.key, file_size as i64);
        if let Some(etag) = response.e_tag() {
            info.etag = Some(etag.trim_matches('"').to_string());
        }
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
        content_type: Option<&str>,
        file_size: u64,
        on_progress: impl Fn(u64) + Send,
    ) -> Result<ObjectInfo> {
        use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};

        let config = crate::multipart::MultipartConfig::default();
        let part_size = config.calculate_part_size(file_size);
        let part_buffer_size = usize::try_from(part_size)
            .map_err(|_| Error::General(format!("invalid part size: {part_size}")))?;

        tracing::debug!(file_size, part_size, "Starting multipart upload");

        let mut create_request = self
            .inner
            .create_multipart_upload()
            .bucket(&path.bucket)
            .key(&path.key);

        if let Some(ct) = content_type {
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

        let mut file = tokio::fs::File::open(file_path)
            .await
            .map_err(|e| Error::General(format!("open file '{}': {e}", file_path.display())))?;
        let mut completed_parts = Vec::new();
        let mut part_number: i32 = 1;
        let mut chunk = vec![0u8; part_buffer_size];
        let mut bytes_uploaded: u64 = 0;

        loop {
            let bytes_read = Self::read_next_part(&mut file, file_path, &mut chunk).await?;
            if bytes_read == 0 {
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
        let complete_result = self
            .inner
            .complete_multipart_upload()
            .bucket(&path.bucket)
            .key(&path.key)
            .upload_id(&upload_id)
            .multipart_upload(completed_upload)
            .send()
            .await;

        let complete_response = match complete_result {
            Ok(response) => response,
            Err(e) => {
                tracing::debug!(upload_id = %upload_id, "Attempting to abort multipart upload after completion failure");
                self.abort_multipart_upload_best_effort(path, &upload_id)
                    .await;
                return Err(Error::Network(format!("complete multipart upload: {e}")));
            }
        };

        tracing::debug!("Multipart upload completed");

        let mut info = ObjectInfo::file(&path.key, file_size as i64);
        if let Some(etag) = complete_response.e_tag() {
            info.etag = Some(etag.trim_matches('"').to_string());
        }
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
        if Self::should_use_multipart(file_size) {
            self.put_object_multipart_from_path(
                path,
                file_path,
                content_type,
                file_size,
                on_progress,
            )
            .await
        } else {
            self.put_object_single_part_from_path(path, file_path, content_type, file_size)
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

#[async_trait]
impl ObjectStore for S3Client {
    async fn list_buckets(&self) -> Result<Vec<ObjectInfo>> {
        let response = self
            .inner
            .list_buckets()
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;

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
            if err_str.contains("NotFound") || err_str.contains("NoSuchBucket") {
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

        Ok(ListResult {
            items,
            truncated: response.is_truncated().unwrap_or(false),
            continuation_token: response.next_continuation_token().map(|s| s.to_string()),
        })
    }

    async fn head_object(&self, path: &RemotePath) -> Result<ObjectInfo> {
        let response = self
            .inner
            .head_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .send()
            .await
            .map_err(|e| {
                let err_str = e.to_string();
                if err_str.contains("NotFound") || err_str.contains("NoSuchKey") {
                    Error::NotFound(path.to_string())
                } else {
                    Error::Network(err_str)
                }
            })?;

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
            .map_err(|e| Error::Network(e.to_string()))?;

        Ok(())
    }

    async fn delete_bucket(&self, bucket: &str) -> Result<()> {
        self.inner
            .delete_bucket()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| {
                let err_str = e.to_string();
                if err_str.contains("NotFound") || err_str.contains("NoSuchBucket") {
                    Error::NotFound(format!("Bucket not found: {bucket}"))
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

    async fn put_object(
        &self,
        path: &RemotePath,
        data: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<ObjectInfo> {
        let size = data.len() as i64;
        let body = aws_sdk_s3::primitives::ByteStream::from(data);

        let mut request = self
            .inner
            .put_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .body(body);

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
        info.last_modified = Some(jiff::Timestamp::now());

        Ok(info)
    }

    async fn delete_object(&self, path: &RemotePath) -> Result<()> {
        self.delete_object_with_options(path, DeleteRequestOptions::default())
            .await
    }

    async fn delete_objects(&self, bucket: &str, keys: Vec<String>) -> Result<Vec<String>> {
        self.delete_objects_with_options(bucket, keys, DeleteRequestOptions::default())
            .await
    }

    async fn copy_object(&self, src: &RemotePath, dst: &RemotePath) -> Result<ObjectInfo> {
        // Build copy source: bucket/key
        let copy_source = format!("{}/{}", src.bucket, src.key);

        let response = self
            .inner
            .copy_object()
            .copy_source(&copy_source)
            .bucket(&dst.bucket)
            .key(&dst.key)
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
            .inner
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

        let mut builder = self.inner.put_object().bucket(&path.bucket).key(&path.key);

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

    async fn list_object_versions(
        &self,
        path: &RemotePath,
        max_keys: Option<i32>,
    ) -> Result<Vec<ObjectVersion>> {
        Ok(self.list_object_versions_page(path, max_keys).await?.items)
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
                    Ok(())
                } else {
                    Err(Error::General(format!("delete_bucket_cors: {error_text}")))
                }
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
    use aws_smithy_http_client::test_util::{CaptureRequestReceiver, capture_request};
    use std::collections::HashMap;

    fn test_s3_client(
        response: Option<http::Response<SdkBody>>,
    ) -> (S3Client, CaptureRequestReceiver) {
        test_s3_client_with_endpoint("https://example.com", response)
    }

    fn test_s3_client_with_endpoint(
        endpoint: &str,
        response: Option<http::Response<SdkBody>>,
    ) -> (S3Client, CaptureRequestReceiver) {
        let (http_client, request_receiver) = capture_request(response);
        let credentials = Credentials::new(
            "access-key",
            "secret-key",
            None,
            None,
            "rc-test-credentials",
        );
        let config = aws_sdk_s3::config::Builder::new()
            .credentials_provider(credentials)
            .endpoint_url(endpoint)
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .force_path_style(true)
            .behavior_version_latest()
            .http_client(http_client)
            .build();

        let alias = Alias::new("test", endpoint, "access-key", "secret-key");
        let client = S3Client {
            inner: aws_sdk_s3::Client::from_conf(config),
            xml_http_client: reqwest::Client::new(),
            alias,
        };

        (client, request_receiver)
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
        let connector = ReqwestConnector::new(true, None).await;
        assert!(
            connector.is_ok(),
            "Expected insecure connector creation to succeed"
        );
    }

    #[tokio::test]
    async fn reqwest_connector_invalid_ca_bundle_path_surfaces_error() {
        // Use an obviously invalid path (empty string) to trigger a read error.
        let result = ReqwestConnector::new(false, Some("")).await;
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
            .delete_object_with_options(&path, DeleteRequestOptions { force_delete: true })
            .await;

        let request = request_receiver.expect_request();
        assert_eq!(request.headers().get("x-rustfs-force-delete"), Some("true"));
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
                DeleteRequestOptions { force_delete: true },
            )
            .await;

        let request = request_receiver.expect_request();
        assert_eq!(request.headers().get("x-rustfs-force-delete"), Some("true"));
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
