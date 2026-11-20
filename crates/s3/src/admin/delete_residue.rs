use super::{AdminClient, BoundedJsonResponse, parse_admin_error};
use async_trait::async_trait;
use rc_core::admin::delete_residue::*;
use rc_core::admin::recovery::validate_recovery_uuid;
use rc_core::admin::validate_local_bucket;
use rc_core::{Alias, Error, Result};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use std::time::Duration;

/// Every request on this transport is single-attempt, including HTTP/2 protocol
/// retries. A lost discard response requires a new observation by the operator.
pub struct DeleteResidueClient(AdminClient);

impl DeleteResidueClient {
    pub fn new(alias: &Alias) -> Result<Self> {
        Ok(Self(AdminClient::with_builder(
            alias,
            reqwest::Client::builder().retry(reqwest::retry::never()),
        )?))
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<T> {
        let request = self.0.request_bounded_bytes_url(
            method,
            self.0.admin_v4_url(path),
            Some(query),
            body,
            BoundedJsonResponse {
                max_bytes: 8 * 1024 * 1024,
                name: "delete residue",
                error_mapper: AdminClient::map_delete_residue_error,
            },
        );
        let bytes = tokio::time::timeout(Duration::from_secs(35), request).await
            .map_err(|_| Error::Network("delete residue request timed out; inspect the exact object before repeating a discard".into()))??;
        serde_json::from_slice(&bytes)
            .map_err(|_| Error::General("invalid delete residue response".into()))
    }
}

impl AdminClient {
    fn map_delete_residue_error(&self, status: StatusCode, body: &str) -> Error {
        match status {
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => Error::Auth("permission denied for delete residue administration".into()),
            StatusCode::NOT_FOUND if parse_admin_error(body).and_then(|e| e.code).as_deref() == Some("NoSuchBucket") =>
                Error::NotFound("bucket not found".into()),
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED =>
                Error::UnsupportedFeature("server does not support the delete residue Admin API".into()),
            StatusCode::PRECONDITION_FAILED | StatusCode::CONFLICT =>
                Error::Conflict("delete residue cursor or evidence changed; obtain a fresh page or inspect the exact key".into()),
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => Error::InvalidPath("server rejected the delete residue selector".into()),
            _ => Error::Network(format!("delete residue request failed with HTTP {}", status.as_u16())),
        }
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn validate_target(target: &DeleteResidueTarget) -> Result<()> {
    validate_local_bucket(&target.bucket)?;
    if target.object.is_empty() || target.object.len() > 1024 {
        return Err(Error::InvalidPath(
            "expected an exact object key of 1..1024 bytes".into(),
        ));
    }
    Ok(())
}

fn validate_observation(
    value: DeleteResidueInspection,
    target: &DeleteResidueTarget,
    discard: bool,
) -> Result<DeleteResidueInspection> {
    let eligible = value.complete && value.reason == "discard_eligible";
    if &value.target != target
        || validate_recovery_uuid(&value.incarnation).is_err()
        || value.reason.is_empty()
        || (eligible != value.fingerprint.is_some())
        || value
            .fingerprint
            .as_deref()
            .is_some_and(|digest| !valid_digest(digest))
        || (discard && (!value.complete || value.reason != "discarded" || value.reclaimed == 0))
        || (!discard && (value.reason == "discarded" || value.reclaimed != 0))
    {
        return Err(Error::General(
            "invalid or mismatched delete residue observation; inspect the exact key again".into(),
        ));
    }
    Ok(value)
}

#[async_trait]
impl DeleteResidueApi for DeleteResidueClient {
    async fn list(
        &self,
        bucket: &str,
        cursor: Option<&str>,
        limit: u16,
    ) -> Result<DeleteResiduePage> {
        validate_local_bucket(bucket)?;
        if !(1..=64).contains(&limit) {
            return Err(Error::InvalidPath("limit must be between 1 and 64".into()));
        }
        let limit_arg = limit.to_string();
        let mut query = vec![("bucket", bucket), ("limit", limit_arg.as_str())];
        if let Some(cursor) = cursor {
            validate_recovery_uuid(cursor)?;
            query.push(("cursor", cursor));
        }
        let page: DeleteResiduePage = self
            .request(Method::GET, "/heal/delete-residue", &query, None)
            .await?;
        if page.bucket != bucket
            || validate_recovery_uuid(&page.incarnation).is_err()
            || page
                .next_cursor
                .as_deref()
                .is_some_and(|id| validate_recovery_uuid(id).is_err())
            || page.complete != (page.next_cursor.is_none() && page.incomplete_sources == 0)
            || (page.source_status != ResidueSourceStatus::Scanned
                && (page.incomplete_sources == 0 || !page.candidates.is_empty()))
            || page.candidates.len() > usize::from(limit)
            || page.candidates.iter().any(|item| {
                item.object.is_empty()
                    || item.object.len() > 1024
                    || validate_recovery_uuid(&item.transaction).is_err()
            })
        {
            return Err(Error::General(
                "invalid or mismatched delete residue inventory".into(),
            ));
        }
        Ok(page)
    }

    async fn inspect(&self, target: &DeleteResidueTarget) -> Result<DeleteResidueInspection> {
        validate_target(target)?;
        let pool = target.pool.to_string();
        let set = target.set.to_string();
        let query = [
            ("bucket", target.bucket.as_str()),
            ("object", target.object.as_str()),
            ("pool", &pool),
            ("set", &set),
        ];
        validate_observation(
            self.request(Method::GET, "/heal/delete-residue/inspect", &query, None)
                .await?,
            target,
            false,
        )
    }

    async fn discard(
        &self,
        target: &DeleteResidueTarget,
        expected_fingerprint: &str,
    ) -> Result<DeleteResidueInspection> {
        validate_target(target)?;
        if !valid_digest(expected_fingerprint) {
            return Err(Error::InvalidPath("expected-fingerprint must be the 64-character lowercase hex fingerprint from inspection".into()));
        }
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Discard<'a> {
            #[serde(flatten)]
            target: &'a DeleteResidueTarget,
            expected_fingerprint: &'a str,
        }
        let body = serde_json::to_vec(&Discard {
            target,
            expected_fingerprint,
        })?;
        validate_observation(
            self.request(
                Method::POST,
                "/heal/delete-residue/discard",
                &[],
                Some(&body),
            )
            .await?,
            target,
            true,
        )
    }
}
