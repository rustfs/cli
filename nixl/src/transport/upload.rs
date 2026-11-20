use super::*;

impl Transport {
    pub(super) async fn upload_reader(
        &self,
        request: UploadRequest<'_>,
        source: &mut (dyn AsyncRead + Unpin + Send),
    ) -> DataTransportResult<UploadResult> {
        let len = part_len(&request)?;
        let pool = self.pool(request.content_length).await?;
        let mut chunk = pool.reserve().await.map_err(unavailable)?;
        let dest = chunk
            .spare_mut()
            .map_err(native_error)
            .map_err(unavailable)?;
        source
            .read_exact(&mut dest[..len])
            .await
            .map_err(Error::from)
            .map_err(rejected)?;
        if source
            .read(&mut [0_u8; 1])
            .await
            .map_err(Error::from)
            .map_err(rejected)?
            != 0
        {
            return Err(rejected(Error::Conflict(
                "NIXL part source exceeds its declared length".into(),
            )));
        }
        self.send_part(&request, pool, chunk)
            .await?
            .ok_or_else(declined_part)
    }
}

fn declined_part() -> DataTransportError {
    rejected(Error::UnsupportedFeature(
        "The endpoint did not accept the NIXL part".into(),
    ))
}

fn part_len(request: &UploadRequest<'_>) -> DataTransportResult<usize> {
    usize::try_from(request.content_length)
        .ok()
        .filter(|n| (1..=WINDOW).contains(n))
        .ok_or_else(|| {
            unavailable(Error::UnsupportedFeature(
                "Part exceeds the registered NIXL window".into(),
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
        let pool = self.pool(first_part_bytes).await?;
        let mut chunk = pool.reserve().await.map_err(unavailable)?;
        chunk
            .append(&[0])
            .map_err(native_error)
            .map_err(unavailable)?;
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
        Ok(self.send_part(&request, pool, chunk).await?.is_some())
    }

    async fn upload_file_part(
        &self,
        request: UploadRequest<'_>,
        file: Arc<std::fs::File>,
        offset: u64,
    ) -> DataTransportResult<UploadResult> {
        use std::os::unix::fs::FileExt;
        let len = part_len(&request)?;
        let pool = self.pool(request.content_length).await?;
        let mut chunk = pool.reserve().await.map_err(unavailable)?;
        let chunk = tokio::task::spawn_blocking(move || {
            file.read_exact_at(&mut chunk.spare_mut().map_err(native_error)?[..len], offset)?;
            Ok::<_, Error>(chunk)
        })
        .await
        .map_err(|_| rejected(Error::General("NIXL file read was interrupted".into())))?
        .map_err(rejected)?;
        self.send_part(&request, pool, chunk)
            .await?
            .ok_or_else(declined_part)
    }
}

impl Transport {
    async fn send_part(
        &self,
        request: &UploadRequest<'_>,
        pool: &Arc<Pool>,
        chunk: StagingChunk,
    ) -> DataTransportResult<Option<UploadResult>> {
        let UploadOperation::UploadPart {
            upload_id,
            part_number,
        } = request.operation
        else {
            return Err(unavailable(Error::UnsupportedFeature(
                "NIXL writes require an unpublished multipart part".into(),
            )));
        };
        let len = part_len(request)?;
        let mut headers = HeaderMap::new();
        wire::bind(&mut headers, &pool.name, &pool.metadata, chunk.addr(), len)
            .map_err(unavailable)?;
        let query = vec![
            ("uploadId".into(), upload_id.into()),
            ("partNumber".into(), part_number.to_string()),
        ];
        let exposed = Exposed::new(chunk, pool.clone());
        let (response, exposed) = self
            .exchange(
                wire::request(Method::PUT, request.path, query, headers),
                exposed,
            )
            .await?;
        if response.status != StatusCode::OK {
            // RustFS can start reading ahead before it validates the multipart ID.
            return Err(rejected(wire::s3_error(response.status, request.path)));
        }
        if !response.headers.contains_key(RDMA_REPLY_HEADER) {
            drop(exposed.finished());
            return Ok(None);
        }
        wire::receipt(StatusCode::OK, &response.headers, len as u64).map_err(unknown)?;
        drop(exposed.finished());
        let etag = wire::field(&response.headers, "etag")
            .map_err(rejected)?
            .filter(|e| !e.is_empty())
            .ok_or_else(|| rejected(Error::General("NIXL UploadPart returned no ETag".into())))?;
        let mut object = ObjectInfo::file(&request.path.key, len as i64);
        object.etag = Some(etag.into());
        Ok(Some(UploadResult {
            object,
            checksum_sha256: None,
        }))
    }
}
