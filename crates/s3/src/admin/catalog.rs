//! Iceberg REST transport using the existing alias TLS and SigV4 client.

use super::{AdminClient, read_bounded_response_body};
use async_trait::async_trait;
use rc_core::catalog::{CatalogOperation as Op, CatalogRequest, TableCatalogApi};
use rc_core::{Error, Result};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::collections::HashSet;

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::catalog::{CatalogTarget, ResourceKind};
    #[test]
    fn catalog_namespace_encoding_and_register_parent() {
        let request = CatalogRequest::new(
            Op::TableRegister,
            CatalogTarget::parse("local/warehouse/a.b/table", ResourceKind::Table).unwrap(),
        );
        let (method, path) = route(&request).unwrap();
        assert_eq!(method, Method::POST);
        assert_eq!(path, "/warehouse/namespaces/a%1Fb/register");
    }
    #[test]
    fn catalog_ref_cannot_escape_path() {
        let mut request = CatalogRequest::new(
            Op::RefSet,
            CatalogTarget::parse("local/warehouse/ns/table", ResourceKind::Table).unwrap(),
        );
        request.child = Some("../bad".into());
        assert!(route(&request).is_err());
    }
    #[test]
    fn catalog_error_statuses_preserve_exit_classes() {
        for (status, code) in [
            (400, 2),
            (401, 4),
            (403, 4),
            (404, 5),
            (406, 7),
            (409, 6),
            (412, 6),
            (503, 3),
        ] {
            assert_eq!(
                catalog_error(StatusCode::from_u16(status).unwrap(), "unsupported backing")
                    .exit_code(),
                code
            );
        }
    }
}

fn route(request: &CatalogRequest) -> Result<(Method, String)> {
    let t = &request.target;
    rc_core::catalog::validate_warehouse(&t.warehouse)?;
    for segment in &t.namespace {
        rc_core::catalog::validate_segment(segment)?;
    }
    if let Some(name) = &t.name {
        rc_core::catalog::validate_segment(name)?;
    }
    let warehouse = format!("/{}", urlencoding::encode(&t.warehouse));
    let namespaces = format!("{warehouse}/namespaces");
    let ns = format!(
        "{namespaces}/{}",
        urlencoding::encode(&t.namespace.join("\u{1f}"))
    );
    let tables = format!("{ns}/tables");
    let name = urlencoding::encode(t.name.as_deref().unwrap_or(""));
    let table = format!("{tables}/{name}");
    let views = format!("{ns}/views");
    let view = format!("{views}/{name}");
    let child = request.child.as_deref().unwrap_or("");
    if matches!(request.operation, Op::RefSet | Op::RefRemove) {
        rc_core::catalog::validate_segment(child)?;
    }
    if matches!(
        request.operation,
        Op::MaintenanceJobShow | Op::JobHeartbeat | Op::JobQuarantine
    ) && (child.is_empty()
        || child.len() > 256
        || child.contains(['/', '\\', '%'])
        || child == "."
        || child == "..")
    {
        return Err(Error::InvalidPath(
            "Invalid maintenance job identifier".into(),
        ));
    }
    let child = urlencoding::encode(child);
    let (method, path) = match request.operation {
        Op::Config => (Method::GET, "/config".into()),
        Op::WarehouseShow => (Method::GET, format!("/buckets{}", warehouse)),
        Op::WarehouseEnable => (Method::PUT, format!("/buckets{}", warehouse)),
        Op::NamespaceCreate => (Method::POST, namespaces),
        Op::NamespaceList => (Method::GET, namespaces),
        Op::NamespaceShow => (Method::GET, ns),
        Op::NamespaceExists => (Method::HEAD, ns),
        Op::NamespaceUpdate => (Method::POST, format!("{ns}/properties")),
        Op::NamespaceRemove => (Method::DELETE, ns),
        Op::TableCreate => (Method::POST, tables),
        Op::TableList => (Method::GET, tables),
        Op::TableRegister => (Method::POST, format!("{ns}/register")),
        Op::TableShow => (Method::GET, table),
        Op::TableExists => (Method::HEAD, table),
        Op::TableRename => (Method::POST, format!("{warehouse}/tables/rename")),
        Op::TableRemove => (Method::DELETE, table),
        Op::Commit => (Method::POST, table),
        Op::MetadataShow => (Method::GET, format!("{table}/metadata-location")),
        Op::MetadataUpdate => (Method::PUT, format!("{table}/metadata-location")),
        Op::RefList => (Method::GET, format!("{table}/refs")),
        Op::RefSet if ref_requires_absence(request) => (Method::POST, table),
        Op::RefSet => (Method::PUT, format!("{table}/refs/{child}")),
        Op::RefRemove => (Method::DELETE, format!("{table}/refs/{child}")),
        Op::ViewCreate => (Method::POST, views),
        Op::ViewList => (Method::GET, views),
        Op::ViewShow => (Method::GET, view),
        Op::ViewExists => (Method::HEAD, view),
        Op::ViewReplace => (Method::POST, view),
        Op::ViewRemove => (Method::DELETE, view),
        Op::MaintenancePlan | Op::MaintenanceRun => {
            (Method::POST, format!("{table}/maintenance/metadata"))
        }
        Op::MaintenanceConfigShow => (Method::GET, format!("{table}/maintenance/config")),
        Op::MaintenanceConfigSet => (Method::PUT, format!("{table}/maintenance/config")),
        Op::MaintenanceJobShow => (Method::GET, format!("{table}/maintenance/jobs/{child}")),
        Op::JobHeartbeat => (
            Method::POST,
            format!("{table}/maintenance/jobs/{child}/heartbeat"),
        ),
        Op::JobQuarantine => (
            Method::POST,
            format!("{table}/maintenance/jobs/{child}/quarantine"),
        ),
        Op::SchedulerShow => (Method::GET, format!("{table}/maintenance/scheduler")),
        Op::SchedulerRun => (Method::POST, format!("{table}/maintenance/scheduler/run")),
        Op::WorkerRun => (Method::POST, format!("{table}/maintenance/worker/run")),
        Op::Diagnostics => (Method::GET, format!("{table}/catalog/diagnostics")),
        Op::Export => (Method::GET, format!("{table}/catalog/export")),
        Op::Import => (Method::POST, format!("{table}/catalog/import")),
        Op::Recover => (Method::POST, format!("{table}/catalog/recovery")),
        Op::Rollback => (Method::POST, format!("{table}/catalog/rollback")),
        Op::ExternalShow => (Method::GET, format!("{table}/catalog/external")),
        Op::ExternalSet => (Method::PUT, format!("{table}/catalog/external")),
        Op::ExternalSync => (Method::POST, format!("{table}/catalog/external/sync")),
        Op::MigrationStatus => (Method::GET, format!("{warehouse}/catalog/migration")),
        Op::MigrationStart => (Method::POST, format!("{warehouse}/catalog/migration")),
        Op::MigrationCancel => (Method::DELETE, format!("{warehouse}/catalog/migration")),
    };
    Ok((method, path))
}

fn ref_requires_absence(request: &CatalogRequest) -> bool {
    request.operation == Op::RefSet
        && request
            .body
            .as_ref()
            .and_then(|v| v.get("expected-snapshot-id"))
            .is_some_and(Value::is_null)
}
fn catalog_wire_body(request: &CatalogRequest) -> Option<Value> {
    if !ref_requires_absence(request) {
        return request.body.clone();
    }
    // The ref endpoint deserializes null as None. A standard requirement keeps
    // null explicit, so create-if-absent cannot overwrite an existing ref.
    let mut update = request.body.clone()?;
    let map = update.as_object_mut()?;
    let commit_id = map.remove("commit-id");
    map.remove("expected-snapshot-id");
    map.insert("action".into(), json!("set-snapshot-ref"));
    map.insert("ref-name".into(), json!(request.child));
    Some(
        json!({"commit-id":commit_id,"requirements":[{"type":"assert-ref-snapshot-id","ref":request.child,"snapshot-id":null}],"updates":[update]}),
    )
}

fn catalog_error(status: StatusCode, message: &str) -> Error {
    let message = format!("Catalog HTTP {}: {}", status.as_u16(), message);
    match status.as_u16() {
        400 | 422 => Error::Config(message),
        401 | 403 => Error::Auth(message),
        404 => Error::NotFound(message),
        406 | 501 => Error::UnsupportedFeature(message),
        409 | 412 => Error::Conflict(message),
        408 | 429 | 500 | 502 | 503 | 504 => Error::Network(message),
        _ => Error::General(message),
    }
}

// Credentials belong to protocol configuration maps, not arbitrary metadata keys.
fn remove_credentials(value: &mut Value) {
    let Some(response) = value.as_object_mut() else {
        return;
    };
    response.remove("storage-credentials");
    for field in ["config", "defaults", "overrides"] {
        if let Some(config) = response.get_mut(field).and_then(Value::as_object_mut) {
            for key in [
                "s3.access-key-id",
                "s3.secret-access-key",
                "s3.session-token",
            ] {
                config.remove(key);
            }
        }
    }
}

impl AdminClient {
    async fn catalog_page(&self, request: &CatalogRequest) -> Result<Value> {
        let (method, path) = route(request)?;
        let mut url = url::Url::parse(&format!("{}/iceberg/v1{path}", self.endpoint))?;
        {
            let mut query = url.query_pairs_mut();
            if request.operation == Op::Config {
                query.append_pair("warehouse", &request.target.warehouse);
            }
            if request.operation == Op::NamespaceList && !request.target.namespace.is_empty() {
                query.append_pair("parent", &request.target.namespace.join("\u{1f}"));
            }
            if request.operation.is_list() {
                query.append_pair("pageSize", &request.page_size.to_string());
                if let Some(token) = &request.page_token {
                    query.append_pair("pageToken", token);
                }
            }
            if let Some(snapshots) = &request.snapshots {
                query.append_pair("snapshots", snapshots);
            }
            if request.operation == Op::TableRemove {
                query.append_pair("purgeRequested", "false");
            }
        }
        let wire_body = catalog_wire_body(request);
        let body = wire_body
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()?
            .unwrap_or_default();
        if body.len() > 8 * 1024 * 1024 {
            return Err(Error::Config("Catalog request exceeds 8 MiB".into()));
        }
        let headers = self.request_headers(&body)?;
        let headers = self
            .sign_request(&method, url.as_str(), &headers, &body)
            .await?;
        let write = !matches!(method, Method::GET | Method::HEAD);
        let response = self
            .http_client
            .request(method.clone(), url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|_| {
                Error::Network(
                    if write {
                        "Catalog mutation outcome unknown; request was not retried"
                    } else {
                        "Catalog request failed"
                    }
                    .into(),
                )
            })?;
        let status = response.status();
        if !status.is_success() {
            let bytes = read_bounded_response_body(response, 64 * 1024, "Catalog error").await?;
            let parsed = serde_json::from_slice::<Value>(&bytes).ok();
            let mut message = parsed
                .as_ref()
                .and_then(|v| v.pointer("/error/message"))
                .and_then(Value::as_str)
                .unwrap_or("Server rejected catalog operation")
                .chars()
                .take(2048)
                .collect::<String>();
            self.redact_admin_credentials(&mut message);
            // Do not echo credential-bearing server diagnostics.
            if ["secret", "credential", "authorization", "token="]
                .iter()
                .any(|word| message.to_ascii_lowercase().contains(word))
            {
                message =
                    "Server rejected catalog operation; check permissions and configuration".into();
            }
            return Err(catalog_error(status, &message));
        }
        if method == Method::HEAD {
            return Ok(json!({"exists": true}));
        }
        if status == StatusCode::NO_CONTENT {
            return Ok(json!({}));
        }
        let bytes =
            read_bounded_response_body(response, 64 * 1024 * 1024, "Catalog response").await?;
        let mut value: Value = if bytes.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(&bytes)
                .map_err(|_| Error::General("Invalid catalog JSON response".into()))?
        };
        if !value.is_object()
            && !(request.operation == Op::MaintenanceConfigShow && value.is_null())
        {
            return Err(Error::General("Catalog response must be an object".into()));
        }
        if request.operation.is_list() {
            let field = if request.operation == Op::NamespaceList {
                "namespaces"
            } else {
                "identifiers"
            };
            if !value.get(field).is_some_and(Value::is_array) {
                return Err(Error::General(format!("Catalog listing missing {field}")));
            }
            if value
                .get("next-page-token")
                .is_some_and(|token| !token.is_null() && !token.is_string())
            {
                return Err(Error::General("Invalid catalog pagination token".into()));
            }
        }
        remove_credentials(&mut value);
        self.redact_admin_credentials_in_value(&mut value);
        Ok(value)
    }
}

#[async_trait]
impl TableCatalogApi for AdminClient {
    async fn catalog(&self, request: &CatalogRequest) -> Result<Value> {
        if !(1..=1000).contains(&request.page_size) {
            return Err(Error::Config("page-size must be between 1 and 1000".into()));
        }
        let mut next = request.clone();
        let mut result = self.catalog_page(&next).await?;
        if !request.operation.is_list() || request.single_page {
            return Ok(result);
        }
        let field = if request.operation == Op::NamespaceList {
            "namespaces"
        } else {
            "identifiers"
        };
        let mut seen = HashSet::new();
        if let Some(token) = &request.page_token {
            seen.insert(token.clone());
        }
        let mut bytes = serde_json::to_vec(&result)?.len();
        loop {
            if !result.get(field).is_some_and(Value::is_array) {
                return Err(Error::General(format!("Catalog listing missing {field}")));
            }
            let token = match result.get("next-page-token") {
                None | Some(Value::Null) => break,
                Some(Value::String(token)) if token.is_empty() => break,
                Some(Value::String(token)) => token.clone(),
                _ => return Err(Error::General("Invalid catalog pagination token".into())),
            };
            if token.len() > 16 * 1024 || !seen.insert(token.clone()) || seen.len() > 10000 {
                return Err(Error::General(
                    "Catalog pagination did not make bounded progress".into(),
                ));
            }
            next.page_token = Some(token);
            let page = self.catalog_page(&next).await?;
            bytes = bytes.saturating_add(serde_json::to_vec(&page)?.len());
            if bytes > 64 * 1024 * 1024 {
                return Err(Error::General(
                    "Catalog listing exceeds 64 MiB; use --no-paginate".into(),
                ));
            }
            let items = page
                .get(field)
                .and_then(Value::as_array)
                .ok_or_else(|| Error::General(format!("Catalog listing missing {field}")))?;
            result
                .get_mut(field)
                .and_then(Value::as_array_mut)
                .ok_or_else(|| Error::General("Invalid catalog listing".into()))?
                .extend(items.iter().cloned());
            result["next-page-token"] = page.get("next-page-token").cloned().unwrap_or(Value::Null);
        }
        if let Some(map) = result.as_object_mut() {
            map.remove("next-page-token");
        }
        Ok(result)
    }
}

#[cfg(test)]
mod transport_tests;
