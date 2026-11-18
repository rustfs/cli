use super::{AdminClient, BoundedJsonResponse};
use async_trait::async_trait;
use rc_core::admin::recovery::*;
use rc_core::{Error, Result};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde_json::Value;

const MRF_ROOT: &str = "/heal/mrf/cluster";

impl AdminClient {
    async fn recovery_json<T: DeserializeOwned>(
        &self,
        method: Method,
        v4: bool,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<T> {
        self.recovery_json_with_budget(
            method,
            (v4, path),
            query,
            body,
            std::time::Duration::from_secs(35),
        )
        .await
    }

    pub(super) async fn recovery_json_with_budget<T: DeserializeOwned>(
        &self,
        method: Method,
        (v4, path): (bool, &str),
        query: &[(&str, &str)],
        body: Option<Vec<u8>>,
        budget: std::time::Duration,
    ) -> Result<T> {
        let url = if v4 {
            self.admin_v4_url(path)
        } else {
            self.admin_url(path)
        };
        let bytes = tokio::time::timeout(budget, self.request_bounded_bytes_url(method, url, Some(query), body.as_deref(), BoundedJsonResponse {
            max_bytes: 8 * 1024 * 1024, name: "recovery", error_mapper: Self::map_recovery_error,
        })).await.map_err(|_| Error::Network("recovery request exceeded its total time budget; reconcile mutations by the original ID before retrying".into()))??;
        // Deserialization errors may include attacker-controlled field or variant names.
        serde_json::from_slice(&bytes)
            .map_err(|_| Error::General("invalid recovery response format".into()))
    }

    fn map_recovery_error(&self, status: reqwest::StatusCode, body: &str) -> Error {
        use reqwest::StatusCode;
        let code = super::parse_admin_error(body).and_then(|error| error.code);
        match status {
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => Error::Auth("permission denied for recovery administration".into()),
            StatusCode::NOT_FOUND if matches!(code.as_deref(), Some("NoSuchBucket" | "NoSuchKey" | "NoSuchObject")) => Error::NotFound("integrity bucket or job was not found".into()),
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED => Error::UnsupportedFeature("this server does not support the requested recovery API".into()),
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => Error::InvalidPath("server rejected the recovery selector, cursor, action, or job; refresh topology and review the documented eligibility limits".into()),
            StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => Error::Conflict("job, target, or bucket identity changed; inspect the saved job".into()),
            _ => Error::Network(format!("recovery request failed with HTTP {}; inspect the same responsibility or job before retrying a mutation", status.as_u16())),
        }
    }
}

fn node_id(value: &str) -> Result<()> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(Error::InvalidPath(
            "nodeId must come from an MRF observation".into(),
        ))
    }
}

fn bucket_path(bucket: &str) -> Result<String> {
    rc_core::admin::validate_local_bucket(bucket)?;
    Ok(format!("/integrity/{}", urlencoding::encode(bucket)))
}

fn observation(
    value: MrfObservation,
    kind: &str,
    target: Option<(&str, &str)>,
) -> Result<MrfObservation> {
    if value.contract_version != 1 {
        return Err(Error::UnsupportedFeature(
            "unsupported MRF observation contract".into(),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    if value.expected_nodes == 0
        || value.nodes.len() > value.expected_nodes
        || value
            .nodes
            .iter()
            .any(|node| node_id(&node.node_id).is_err() || !seen.insert(&node.node_id))
        || target.is_some_and(|(node, _)| value.nodes.len() != 1 || value.nodes[0].node_id != node)
    {
        return Err(Error::General("invalid MRF node coverage".into()));
    }
    for node in &value.nodes {
        if node.state != "ok" {
            continue;
        }
        let valid = match (&node.reply, kind) {
            (Some(MrfReply::Summary { summary }), "summary") => {
                summary.contract_version == 1 && summary.checkpoint_readable
            }
            (Some(MrfReply::List { snapshot, .. }), "list") => {
                snapshot.metadata.get("contractVersion") == Some(&Value::from(2))
                    && snapshot.metadata.get("nodeLocal") == Some(&Value::Bool(true))
            }
            (Some(MrfReply::Get { observation }), "get") => {
                observation.contract_version == 1
                    && observation.responsibility.as_ref().is_none_or(|row| {
                        target.is_some_and(|(_, id)| row.responsibility_id.eq_ignore_ascii_case(id))
                    })
            }
            (Some(MrfReply::Action { action }), "action") => {
                !action.data_verified
                    && action.durable_responsibility_retained
                    && target
                        .is_some_and(|(_, id)| action.responsibility_id.eq_ignore_ascii_case(id))
            }
            _ => false,
        };
        if !valid {
            return Err(Error::General(
                "MRF response does not match the requested observation".into(),
            ));
        }
    }
    Ok(value)
}

fn job(value: IntegrityJob, bucket: &str, id: Option<&str>) -> Result<IntegrityJob> {
    if value.format_version != 1 {
        return Err(Error::UnsupportedFeature(
            "unsupported integrity job format".into(),
        ));
    }
    if value.bucket != bucket
        || id.is_some_and(|id| !value.id.eq_ignore_ascii_case(id))
        || validate_recovery_uuid(&value.id).is_err()
        || validate_recovery_uuid(&value.bucket_incarnation).is_err()
        || value.results.len() != value.request.items.len()
        || !(1..=64).contains(&value.results.len())
        || !["audit", "migrate"].contains(&value.request.mode.as_str())
    {
        return Err(Error::General(
            "integrity result does not identify the requested job".into(),
        ));
    }
    Ok(value)
}

#[async_trait]
impl RecoveryApi for AdminClient {
    async fn mrf_summary(&self, cursor: Option<&str>) -> Result<MrfObservation> {
        let query: Vec<_> = cursor
            .map(|cursor| ("cursor", cursor))
            .into_iter()
            .collect();
        observation(
            self.recovery_json(Method::GET, true, MRF_ROOT, &query, None)
                .await?,
            "summary",
            None,
        )
    }

    async fn mrf_list(&self, cursor: Option<&str>, limit: u16) -> Result<MrfObservation> {
        if !(1..=256).contains(&limit) {
            return Err(Error::InvalidPath("MRF page limit must be 1..256".into()));
        }
        let limit = limit.to_string();
        let mut query = vec![("limit", limit.as_str())];
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor));
        }
        observation(
            self.recovery_json(
                Method::GET,
                true,
                &format!("{MRF_ROOT}/responsibilities"),
                &query,
                None,
            )
            .await?,
            "list",
            None,
        )
    }

    async fn mrf_get(&self, node: &str, id: &str) -> Result<MrfObservation> {
        node_id(node)?;
        validate_recovery_uuid(id)?;
        observation(
            self.recovery_json(
                Method::GET,
                true,
                &format!("{MRF_ROOT}/responsibilities/{node}/{id}"),
                &[],
                None,
            )
            .await?,
            "get",
            Some((node, id)),
        )
    }

    async fn mrf_action(&self, action: &MrfAction) -> Result<MrfObservation> {
        node_id(&action.node_id)?;
        validate_recovery_uuid(&action.request.responsibility_id)?;
        if let Some(id) = &action.request.expected_bucket_incarnation_id {
            validate_recovery_uuid(id)?;
        }
        observation(
            self.recovery_json(
                Method::POST,
                true,
                &format!("{MRF_ROOT}/responsibilities/actions"),
                &[],
                Some(serde_json::to_vec(action)?),
            )
            .await?,
            "action",
            Some((&action.node_id, &action.request.responsibility_id)),
        )
    }

    async fn integrity_readiness(&self) -> Result<Value> {
        self.recovery_json(Method::GET, false, "/integrity/readiness", &[], None)
            .await
    }

    async fn integrity_inventory(
        &self,
        bucket: &str,
        query: &IntegrityInventoryQuery,
    ) -> Result<IntegrityInventory> {
        if !(1..=100).contains(&query.limit)
            || query.prefix.len() > 1024
            || (query.version_marker.is_some() && query.key_marker.is_none())
        {
            return Err(Error::InvalidPath("inventory limit must be 1..100, prefix at most 1024 bytes, and version marker requires key marker".into()));
        }
        let limit = query.limit.to_string();
        let mut params = vec![("prefix", query.prefix.as_str()), ("limit", limit.as_str())];
        if let Some(marker) = &query.key_marker {
            params.push(("key-marker", marker));
        }
        if let Some(marker) = &query.version_marker {
            params.push(("version-marker", marker));
        }
        self.recovery_json(
            Method::GET,
            false,
            &format!("{}/inventory", bucket_path(bucket)?),
            &params,
            None,
        )
        .await
    }

    async fn integrity_create(
        &self,
        bucket: &str,
        request: &IntegrityJobRequest,
    ) -> Result<IntegrityJob> {
        let mut request = request.clone();
        request.validate_and_normalize()?;
        job(
            self.recovery_json(
                Method::POST,
                false,
                &format!("{}/jobs", bucket_path(bucket)?),
                &[],
                Some(serde_json::to_vec(&request)?),
            )
            .await?,
            bucket,
            None,
        )
    }

    async fn integrity_job(&self, bucket: &str, id: &str) -> Result<IntegrityJob> {
        validate_recovery_uuid(id)?;
        job(
            self.recovery_json(
                Method::GET,
                false,
                &format!("{}/jobs/{id}", bucket_path(bucket)?),
                &[],
                None,
            )
            .await?,
            bucket,
            Some(id),
        )
    }

    async fn integrity_control(
        &self,
        bucket: &str,
        id: &str,
        operation: &str,
    ) -> Result<IntegrityJob> {
        validate_recovery_uuid(id)?;
        if !["pause", "resume", "cancel"].contains(&operation) {
            return Err(Error::InvalidPath(
                "expected pause, resume or cancel".into(),
            ));
        }
        job(
            self.recovery_json(
                Method::POST,
                false,
                &format!("{}/jobs/{id}/control", bucket_path(bucket)?),
                &[],
                Some(serde_json::to_vec(
                    &serde_json::json!({"operation":operation}),
                )?),
            )
            .await?,
            bucket,
            Some(id),
        )
    }
}
