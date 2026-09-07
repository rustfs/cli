//! On-Demand Migration admin transport over the existing SigV4 client.
//!
//! Every route lives under `/rustfs/admin/v3/on-demand-migration/{bucket}`.
//! The `PUT` body carries a plaintext source secret, so it is built once in
//! zeroizing storage, handed to the HTTP client by ownership, and never
//! echoed: a transport failure reports a fixed message rather than the
//! request, and server error bodies are bounded and credential-scrubbed.

use super::{AdminClient, SensitiveRequestBody, parse_admin_error, read_bounded_response_body};
use async_trait::async_trait;
use bytes::Bytes;
use rc_core::admin::{
    BackfillJobResult, BackfillStartRequest, MAX_ON_DEMAND_MIGRATION_RESPONSE_BYTES,
    OnDemandMigrationApi, OnDemandMigrationConfigRequest, OnDemandMigrationConfigResult,
    OnDemandMigrationSetResult, OnDemandMigrationStatus, validate_local_bucket,
};
use rc_core::{Error, Result};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use zeroize::Zeroizing;

/// Message for a 404 that is not one of the route's own not-found answers:
/// the whole route family is absent, so the server predates the feature.
pub const UNSUPPORTED_MESSAGE: &str = "server does not support on-demand migration";

/// Error codes the route family answers 404 with when the route itself exists.
const KNOWN_NOT_FOUND_CODES: &[&str] =
    &["NoSuchConfiguration", "NoSuchBucket", "NoSuchBackfillJob"];

/// `400` code meaning the probe could not reach the source: a network class,
/// not a usage class, because the configuration itself was accepted.
const SOURCE_UNREACHABLE_CODE: &str = "OnDemandMigrationSourceUnreachable";

/// Longest server message echoed back to the operator.
const MAX_ERROR_MESSAGE_CHARS: usize = 512;

impl AdminClient {
    fn on_demand_migration_url(
        &self,
        bucket: &str,
        suffix: &str,
        query: &[(&str, &str)],
    ) -> String {
        let mut url = self.admin_url(&format!(
            "/on-demand-migration/{}{suffix}",
            urlencoding::encode(bucket)
        ));
        let query_string = query
            .iter()
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    urlencoding::encode(key),
                    urlencoding::encode(value)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        if !query_string.is_empty() {
            url.push('?');
            url.push_str(&query_string);
        }
        url
    }

    /// One signed request to the route family with a bounded response.
    ///
    /// The body, when present, is owned by the request so the only plaintext
    /// copy of the secret is wiped when the HTTP client drops it.
    async fn on_demand_migration_request(
        &self,
        method: Method,
        bucket: &str,
        suffix: &str,
        query: &[(&str, &str)],
        body: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<(StatusCode, Vec<u8>)> {
        validate_local_bucket(bucket)?;
        let url = self.on_demand_migration_url(bucket, suffix, query);
        let body_bytes = body
            .as_ref()
            .map(|body| body.as_slice())
            .unwrap_or_default();
        let headers = self.request_headers(body_bytes)?;
        let signed_headers = self
            .sign_request(&method, &url, &headers, body_bytes)
            .await?;
        let write = !matches!(method, Method::GET | Method::HEAD);
        let mut request = self.http_client.request(method, &url);
        for (name, value) in signed_headers.iter() {
            request = request.header(name, value);
        }
        if let Some(body) = body {
            request = request.body(Bytes::from_owner(SensitiveRequestBody(body)));
        }
        // A fixed message: the reqwest error can embed the URL, and a write
        // whose outcome is unknown must not be retried by a caller.
        let response = request.send().await.map_err(|_| {
            Error::Network(if write {
                "On-demand migration request failed; outcome unknown and not retried".to_string()
            } else {
                "On-demand migration request failed".to_string()
            })
        })?;
        let status = response.status();
        let bytes = read_bounded_response_body(
            response,
            MAX_ON_DEMAND_MIGRATION_RESPONSE_BYTES,
            "On-demand migration response",
        )
        .await?;
        if !status.is_success() {
            let mut body_text = String::from_utf8_lossy(&bytes).into_owned();
            self.redact_admin_credentials(&mut body_text);
            return Err(map_on_demand_migration_error(status, &body_text));
        }
        Ok((status, bytes))
    }

    async fn on_demand_migration_json<T: DeserializeOwned>(
        &self,
        method: Method,
        bucket: &str,
        suffix: &str,
        query: &[(&str, &str)],
        body: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<T> {
        let (_, bytes) = self
            .on_demand_migration_request(method, bucket, suffix, query, body)
            .await?;
        if bytes.is_empty() {
            return Err(Error::General(
                "On-demand migration response was empty".to_string(),
            ));
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| Error::General("Invalid on-demand migration JSON response".to_string()))
    }
}

/// Map the route family's answers onto exit classes.
///
/// | Answer | Class | Exit |
/// |---|---|---|
/// | 400 `OnDemandMigrationSourceUnreachable` | network | 3 |
/// | 400 anything else (validation, module switch off) | usage | 2 |
/// | 401 / 403 (unauthorized, or the licence denies the entitlement) | auth | 4 |
/// | 404 with a known code (no config, no bucket, no job) | not found | 5 |
/// | 404 otherwise (route family absent) | unsupported | 7 |
/// | 409 (a backfill job holds the lease) | conflict | 6 |
/// | 501 (provider excluded at build time) | unsupported | 7 |
/// | 408 / 429 / 5xx | network | 3 |
pub(crate) fn map_on_demand_migration_error(status: StatusCode, body: &str) -> Error {
    let structured = parse_admin_error(body);
    let code = structured.as_ref().and_then(|error| error.code.clone());
    let message = structured
        .as_ref()
        .and_then(|error| error.message.clone())
        .map(|message| sanitize_message(&message))
        .filter(|message| !message.is_empty());
    let describe = |fallback: &str| {
        let mut text = format!("HTTP {}", status.as_u16());
        if let Some(code) = &code {
            text.push(' ');
            text.push_str(&sanitize_message(code));
        }
        text.push_str(": ");
        text.push_str(message.as_deref().unwrap_or(fallback));
        text
    };
    match status {
        StatusCode::BAD_REQUEST if code.as_deref() == Some(SOURCE_UNREACHABLE_CODE) => {
            Error::Network(describe("the source bucket did not answer the probe"))
        }
        StatusCode::BAD_REQUEST => Error::Config(describe("the request was rejected")),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Error::Auth(describe(
            "the admin action is not authorized or the licence denies it",
        )),
        StatusCode::NOT_FOUND
            if code
                .as_deref()
                .is_some_and(|code| KNOWN_NOT_FOUND_CODES.contains(&code)) =>
        {
            Error::NotFound(describe("not found"))
        }
        StatusCode::NOT_FOUND => Error::UnsupportedFeature(UNSUPPORTED_MESSAGE.to_string()),
        StatusCode::CONFLICT => Error::Conflict(describe("a backfill job is already running")),
        StatusCode::NOT_IMPLEMENTED => {
            Error::UnsupportedFeature(describe("the provider is not compiled into this server"))
        }
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS => {
            Error::Network(describe("the server asked for a retry"))
        }
        status if status.is_server_error() => Error::Network(describe("server error")),
        _ => Error::General(describe("unexpected response")),
    }
}

/// Bound and de-control a server-authored string before it reaches a terminal.
fn sanitize_message(message: &str) -> String {
    message
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_ERROR_MESSAGE_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

#[async_trait]
impl OnDemandMigrationApi for AdminClient {
    async fn set_on_demand_migration(
        &self,
        bucket: &str,
        config: &OnDemandMigrationConfigRequest,
        dry_run: bool,
    ) -> Result<OnDemandMigrationSetResult> {
        validate_local_bucket(bucket)?;
        let body = config.to_wire_json()?;
        let query: &[(&str, &str)] = if dry_run { &[("dry-run", "true")] } else { &[] };
        self.on_demand_migration_json(Method::PUT, bucket, "", query, Some(body))
            .await
    }

    async fn get_on_demand_migration(&self, bucket: &str) -> Result<OnDemandMigrationConfigResult> {
        self.on_demand_migration_json(Method::GET, bucket, "", &[], None)
            .await
    }

    async fn delete_on_demand_migration(&self, bucket: &str) -> Result<()> {
        self.on_demand_migration_request(Method::DELETE, bucket, "", &[], None)
            .await
            .map(|_| ())
    }

    async fn on_demand_migration_status(&self, bucket: &str) -> Result<OnDemandMigrationStatus> {
        self.on_demand_migration_json(Method::GET, bucket, "/status", &[], None)
            .await
    }

    async fn start_on_demand_migration_backfill(
        &self,
        bucket: &str,
        request: &BackfillStartRequest,
    ) -> Result<BackfillJobResult> {
        request.validate()?;
        let body = Zeroizing::new(serde_json::to_vec(request)?);
        self.on_demand_migration_json(
            Method::POST,
            bucket,
            "/backfill",
            &[("op", "start")],
            Some(body),
        )
        .await
    }

    async fn cancel_on_demand_migration_backfill(&self, bucket: &str) -> Result<BackfillJobResult> {
        self.on_demand_migration_json(Method::POST, bucket, "/backfill", &[("op", "cancel")], None)
            .await
    }

    async fn on_demand_migration_backfill_status(&self, bucket: &str) -> Result<BackfillJobResult> {
        self.on_demand_migration_json(Method::GET, bucket, "/backfill", &[], None)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_map_to_the_documented_exit_classes() {
        let cases: &[(u16, &str, i32)] = &[
            (
                400,
                r#"{"Code":"InvalidArgument","Message":"bad endpoint"}"#,
                2,
            ),
            (
                400,
                r#"{"Code":"OnDemandMigrationDisabled","Message":"off"}"#,
                2,
            ),
            (
                400,
                r#"{"Code":"OnDemandMigrationSourceUnreachable","Message":"connect"}"#,
                3,
            ),
            (401, "", 4),
            (403, r#"{"Code":"AccessDenied","Message":"licence"}"#, 4),
            (
                404,
                r#"{"Code":"NoSuchConfiguration","Message":"unset"}"#,
                5,
            ),
            (404, r#"{"Code":"NoSuchBucket","Message":"missing"}"#, 5),
            (
                404,
                r#"{"Code":"NoSuchBackfillJob","Message":"never ran"}"#,
                5,
            ),
            (404, "", 7),
            (404, "<html>not found</html>", 7),
            (
                409,
                r#"{"Code":"OnDemandMigrationBackfillRunning","Message":"busy"}"#,
                6,
            ),
            (501, r#"{"Code":"OnDemandMigrationBackendNotCompiled"}"#, 7),
            (429, "", 3),
            (503, "", 3),
            (418, "", 1),
        ];
        for (status, body, exit_code) in cases {
            let error = map_on_demand_migration_error(StatusCode::from_u16(*status).unwrap(), body);
            assert_eq!(error.exit_code(), *exit_code, "{status} {body}");
        }
    }

    #[test]
    fn route_absence_uses_the_fixed_unsupported_message() {
        let error = map_on_demand_migration_error(StatusCode::NOT_FOUND, "");
        assert_eq!(
            error.to_string(),
            format!("Unsupported feature: {UNSUPPORTED_MESSAGE}")
        );
    }

    #[test]
    fn server_messages_are_bounded_and_de_controlled() {
        let body = format!(
            r#"{{"Code":"InvalidArgument","Message":"line\u001b[31mred\n{}"}}"#,
            "x".repeat(2000)
        );
        let text = map_on_demand_migration_error(StatusCode::BAD_REQUEST, &body).to_string();
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\n'));
        assert!(text.len() < 700);
        assert!(text.contains("HTTP 400 InvalidArgument"));
    }
}

#[cfg(test)]
mod transport_tests;
