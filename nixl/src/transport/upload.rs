use super::*;

impl Transport {
    pub(super) async fn upload_reader(
        &self,
        request: UploadRequest<'_>,
        source: &mut (dyn AsyncRead + Unpin + Send),
    ) -> DataTransportResult<UploadResult> {
        let len = part_len(&request)?;
        let mut window = self
            .pools
            .reserve(request.content_length)
            .await
            .map_err(unavailable)?;
        let dest = window.writable().map_err(unavailable)?;
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
        let mut window = self
            .pools
            .reserve(first_part_bytes)
            .await
            .map_err(unavailable)?;
        window.writable().map_err(unavailable)?[0] = 0;
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
        use std::os::unix::fs::FileExt;
        let len = part_len(&request)?;
        let mut window = self
            .pools
            .reserve(request.content_length)
            .await
            .map_err(unavailable)?;
        let window = tokio::task::spawn_blocking(move || {
            file.read_exact_at(&mut window.writable()?[..len], offset)?;
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
        let mut headers = HeaderMap::new();
        let exposed = window
            .expose(&Method::PUT, len, &mut headers)
            .map_err(unavailable)?;
        let query = vec![
            ("uploadId".into(), upload_id.into()),
            ("partNumber".into(), part_number.to_string()),
        ];
        let (response, exposed) = self
            .exchange(
                wire::request(Method::PUT, request.path, query, headers),
                exposed,
            )
            .await?;
        if response.status != StatusCode::OK {
            // RustFS can start reading ahead before it validates the multipart ID.
            return Err(rejected(response.object_error(request.path, None)));
        }
        if !response.headers.contains_key(RDMA_REPLY_HEADER) {
            // A missing receipt permits the S3 fallback but does not prove the window is idle.
            return Ok(None);
        }
        wire::receipt(StatusCode::OK, &response.headers, len as u64).map_err(unknown)?;
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
