use super::*;

impl Transport {
    pub(super) async fn upload_reader(
        &self,
        request: UploadRequest<'_>,
        source: &mut (dyn AsyncRead + Unpin + Send),
    ) -> DataTransportResult<UploadResult> {
        let len = part_len(&request)?;
        let _probe = self.support.upload_part.enter().await?;
        let mut window = self
            .pools
            .reserve(request.content_length)
            .await
            .map_err(unavailable)?;
        match window.buffer() {
            crate::buffer::Buffer::Nixl(chunk) => {
                let mut scratch = vec![0; len.min(crate::buffer::STREAM_CHUNK)];
                let mut offset = 0;
                while offset < len {
                    let count = scratch.len().min(len - offset);
                    let bytes = &mut scratch[..count];
                    source
                        .read_exact(bytes)
                        .await
                        .map_err(Error::from)
                        .map_err(rejected)?;
                    chunk
                        .write_at(offset, bytes)
                        .map_err(crate::buffer::nixl_error)
                        .map_err(rejected)?;
                    offset += count;
                }
            }
            #[cfg(feature = "cuobject")]
            crate::buffer::Buffer::CuObject(host) => {
                let dest = host.writable().map_err(unavailable)?;
                source
                    .read_exact(&mut dest[..len])
                    .await
                    .map_err(Error::from)
                    .map_err(rejected)?;
            }
        }
        if source
            .read(&mut [0_u8; 1])
            .await
            .map_err(Error::from)
            .map_err(rejected)?
            != 0
        {
            return Err(rejected(Error::Conflict(
                "RDMA part source exceeds its declared length".into(),
            )));
        }
        self.send_part(&request, window)
            .await?
            .ok_or_else(declined_part)
    }
}

fn declined_part() -> DataTransportError {
    rejected(Error::UnsupportedFeature(
        "The endpoint did not accept the RDMA part".into(),
    ))
}

fn part_len(request: &UploadRequest<'_>) -> DataTransportResult<usize> {
    usize::try_from(request.content_length)
        .ok()
        .filter(|n| (1..=WINDOW).contains(n))
        .ok_or_else(|| {
            unavailable(Error::UnsupportedFeature(
                "Part exceeds the registered RDMA window".into(),
            ))
        })
}

#[async_trait]
impl StagedUploadTransport for Transport {
    fn max_part_size(&self) -> u64 {
        WINDOW as u64
    }

    async fn probe(
        &self,
        path: &RemotePath,
        upload_id: &str,
        first_part_bytes: u64,
    ) -> DataTransportResult<bool> {
        let _probe = self.support.upload_part.enter().await?;
        let mut window = self
            .pools
            .reserve(first_part_bytes)
            .await
            .map_err(unavailable)?;
        window.buffer().write_at(0, &[0]).map_err(unavailable)?;
        let options = rc_core::ObjectWriteOptions::default();
        let request = UploadRequest {
            path,
            options: &options,
            content_length: 1,
            operation: UploadOperation::UploadPart {
                upload_id,
                part_number: 1,
            },
            checksum_sha256: None,
        };
        Ok(self.send_part(&request, window).await?.is_some())
    }

    async fn upload_file_part(
        &self,
        request: UploadRequest<'_>,
        file: Arc<std::fs::File>,
        offset: u64,
    ) -> DataTransportResult<UploadResult> {
        let len = part_len(&request)?;
        let _probe = self.support.upload_part.enter().await?;
        let mut window = self
            .pools
            .reserve(request.content_length)
            .await
            .map_err(unavailable)?;
        let window = tokio::task::spawn_blocking(move || {
            window.buffer().read_file(&file, offset, len)?;
            Ok::<_, Error>(window)
        })
        .await
        .map_err(|_| rejected(Error::General("RDMA file read was interrupted".into())))?
        .map_err(rejected)?;
        self.send_part(&request, window)
            .await?
            .ok_or_else(declined_part)
    }
}

impl Transport {
    async fn send_part(
        &self,
        request: &UploadRequest<'_>,
        window: Window,
    ) -> DataTransportResult<Option<UploadResult>> {
        let UploadOperation::UploadPart {
            upload_id,
            part_number,
        } = request.operation
        else {
            return Err(unavailable(Error::UnsupportedFeature(
                "RDMA writes require an unpublished multipart part".into(),
            )));
        };
        let len = part_len(request)?;
        let headers = HeaderMap::new();
        let query = vec![
            ("uploadId".into(), upload_id.into()),
            ("partNumber".into(), part_number.to_string()),
        ];
        let (response, exposed) = self
            .exchange(
                wire::request(Method::PUT, request.path, query, headers),
                window,
                len,
            )
            .await?;
        if response.status != StatusCode::OK {
            // RustFS can start reading ahead before it validates the multipart ID.
            return Err(rejected(response.object_error(request.path, None)));
        }
        if !response.headers.contains_key(RDMA_REPLY_HEADER) {
            // A missing receipt permits the S3 fallback but does not prove the window is idle.
            self.support.upload_part.http_only();
            return Ok(None);
        }
        wire::receipt(StatusCode::OK, &response.headers, len as u64).map_err(unknown)?;
        self.support.upload_part.supported();
        drop(exposed.finished().map_err(rejected)?);
        let etag = wire::field(&response.headers, "etag")
            .map_err(rejected)?
            .filter(|e| !e.is_empty())
            .ok_or_else(|| rejected(Error::General("RDMA UploadPart returned no ETag".into())))?;
        let mut object = ObjectInfo::file(&request.path.key, len as i64);
        object.etag = Some(etag.into());
        Ok(Some(UploadResult {
            object,
            checksum_sha256: None,
        }))
    }
}
