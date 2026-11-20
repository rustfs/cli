//! Publish native buffer uploads only after every unpublished S3 part has a receipt.

use super::*;
use crate::data_transport::StagedUploadTransport;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use futures::{StreamExt, stream::FuturesUnordered};

pub(super) enum Source<'a> {
    File(Arc<std::fs::File>),
    Reader(&'a mut (dyn tokio::io::AsyncRead + Unpin + Send)),
}

/// Cancellation drops the caller's future before its ordinary error path runs.
struct PendingUpload<'a> {
    client: &'a S3Client,
    path: &'a RemotePath,
    id: Option<String>,
}

impl Drop for PendingUpload<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let client = self.client.clone();
            let path = self.path.clone();
            runtime.spawn(async move {
                client.abort_multipart_upload_best_effort(&path, &id).await;
            });
        }
    }
}

impl<'a> From<WriteCondition<'a>> for ObjectWritePrecondition<'a> {
    fn from(value: WriteCondition<'a>) -> Self {
        match value {
            WriteCondition::None => Self::None,
            WriteCondition::IfAbsent => Self::IfAbsent,
            WriteCondition::IfMatch(etag) => Self::IfMatch(etag),
        }
    }
}

impl S3Client {
    pub(super) async fn start_object_upload(
        &self,
        path: &RemotePath,
        options: PathUploadOptions<'_>,
        checksum_requested: bool,
    ) -> Result<String> {
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
            self.map_sdk_failure(&error, || {
                self.redact_sse_customer_error(
                    Self::map_transfer_write_error(&error, path, options.write),
                    destination_sse_customer_key(options.write.encryption.as_ref()),
                )
            })
        })?;

        Ok(create_response
            .upload_id()
            .ok_or_else(|| Error::General("missing upload id from multipart upload".to_string()))?
            .to_string())
    }

    pub(super) async fn finish_object_upload(
        &self,
        path: &RemotePath,
        upload_id: &str,
        options: PathUploadOptions<'_>,
        completed_parts: Vec<CompletedPart>,
        file_size: u64,
        has_external_parts: bool,
    ) -> Result<ObjectInfo> {
        let completed_upload = CompletedMultipartUpload::builder()
            .set_parts(Some(completed_parts))
            .build();
        let mut complete_request = self
            .inner
            .complete_multipart_upload()
            .bucket(&path.bucket)
            .key(&path.key)
            .upload_id(upload_id)
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
                let primary = if has_external_parts {
                    let state = match &error {
                        aws_sdk_s3::error::SdkError::ServiceError(service_error)
                            if matches!(service_error.raw().status().as_u16(), 409 | 412) =>
                        {
                            rc_core::DataTransferState::ConfirmedFailure
                        }
                        _ => rc_core::DataTransferState::OutcomeUnknown,
                    };
                    Error::DataTransfer {
                        state,
                        message: "Multipart completion did not return a usable success result"
                            .to_string(),
                        source: Some(Box::new(primary)),
                    }
                } else {
                    primary
                };
                return Err(self
                    .abort_multipart_upload_with_error(path, upload_id, primary)
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

        Ok(info)
    }

    pub(super) async fn put_staged_object(
        &self,
        path: &RemotePath,
        source: Source<'_>,
        length: u64,
        options: PathUploadOptions<'_>,
        staged: &dyn StagedUploadTransport,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<Option<ObjectInfo>> {
        let auto = self.data_transport_mode == rc_core::DataTransportMode::Auto;
        let endpoint = if self.alias.anonymous {
            Err(Error::RequestRejected(
                "Native object transfer requires a signed S3 request".into(),
            ))
        } else {
            self.external_control_url(path, &[]).map(|_| ())
        };
        if let Err(error) = endpoint {
            return if auto { Ok(None) } else { Err(error) };
        }
        let part_size = crate::multipart::MultipartConfig::default()
            .checked_part_size(length, rc_core::global_multipart_max_parts())?;
        if options.write.checksum.is_some()
            || options.write.storage_class.is_some()
            || destination_sse_customer_key(options.write.encryption.as_ref()).is_some()
            || part_size.min(length) > staged.max_part_size()
        {
            return if auto {
                Ok(None)
            } else {
                Err(Error::UnsupportedFeature("This part size, storage class, explicit S3 checksum or SSE-C request is unavailable with native multipart transport".into()))
            };
        }
        let upload_id = self.start_object_upload(path, options, false).await?;
        let mut pending = PendingUpload {
            client: self,
            path,
            id: Some(upload_id.clone()),
        };
        let upload = async {
            if auto {
                match staged.probe(path, &upload_id, part_size.min(length)).await {
                    Ok(true) => {}
                    Ok(false) => return Ok(None),
                    Err(error) if error.state == rc_core::DataTransferState::NotAccepted => {
                        return Ok(None);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            let count = length.div_ceil(part_size);
            let part_request = |i: u64| UploadRequest {
                path,
                options: options.write,
                operation: UploadOperation::UploadPart {
                    upload_id: &upload_id,
                    part_number: i as i32 + 1,
                },
                content_length: part_size.min(length - i * part_size),
                checksum_sha256: None,
            };
            let mut parts = Vec::with_capacity(count as usize);
            let mut transferred = 0;
            let mut finish_part = |i: u64, result: UploadResult| -> Result<()> {
                let etag = result
                    .object
                    .etag
                    .ok_or_else(|| Error::General("Native part has no ETag".into()))?;
                parts.push(
                    CompletedPart::builder()
                        .part_number(i as i32 + 1)
                        .e_tag(etag)
                        .build(),
                );
                transferred += part_request(i).content_length;
                progress(transferred);
                Ok(())
            };
            match source {
                Source::File(file) => {
                    const FILE_WINDOWS: usize = 2;
                    let send = |i| {
                        let part = staged.upload_file_part(
                            part_request(i),
                            Arc::clone(&file),
                            i * part_size,
                        );
                        async move { (i, part.await) }
                    };
                    let mut remaining = 0..count;
                    let mut in_flight: FuturesUnordered<_> =
                        remaining.by_ref().take(FILE_WINDOWS).map(send).collect();
                    while let Some((i, result)) = in_flight.next().await {
                        if let Err(error) = result
                            .map_err(Error::from)
                            .and_then(|part| finish_part(i, part))
                        {
                            // Let already-exposed windows receive their receipts before aborting.
                            while in_flight.next().await.is_some() {}
                            return Err(error);
                        }
                        if let Some(next) = remaining.next() {
                            in_flight.push(send(next));
                        }
                    }
                    parts.sort_unstable_by_key(CompletedPart::part_number);
                    if file.metadata()?.len() != length {
                        return Err(Error::Conflict("Upload source length changed".into()));
                    }
                }
                Source::Reader(source) => {
                    for i in 0..count {
                        let request = part_request(i);
                        let mut part = (&mut *source).take(request.content_length);
                        let result = self
                            .upload_with_transport(staged, request, &mut part)
                            .await?
                            .ok_or_else(|| {
                                Error::UnsupportedFeature("Native part transfer declined".into())
                            })?;
                        finish_part(i, result)?;
                    }
                    if source.read(&mut [0_u8; 1]).await? != 0 {
                        return Err(Error::Conflict(
                            "Upload source exceeds its declared length".into(),
                        ));
                    }
                }
            }
            Ok(Some(parts))
        }
        .await;
        let result = match upload {
            Ok(Some(parts)) => self
                .finish_object_upload(path, &upload_id, options, parts, length, true)
                .await
                .map(Some),
            Ok(None) => match self
                .abort_multipart_upload_best_effort(path, &upload_id)
                .await
            {
                MultipartAbortStatus::Succeeded => Ok(None),
                status => Err(
                    Error::General("Could not abort the native upload probe".into())
                        .with_multipart_copy_context(&upload_id, status),
                ),
            },
            Err(error) => Err(self
                .abort_multipart_upload_with_error(path, &upload_id, error)
                .await),
        };
        pending.id = None;
        result
    }
}
