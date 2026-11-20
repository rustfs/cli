use super::*;

pub(super) enum Sink<'a> {
    File(Arc<std::fs::File>),
    Writer(&'a mut (dyn AsyncWrite + Send + Unpin)),
}

struct Selection<'a> {
    path: &'a RemotePath,
    query: Vec<(String, String)>,
    total: u64,
    etag: HeaderValue,
}

impl Transport {
    async fn fetch(
        &self,
        selection: &Selection<'_>,
        offset: u64,
        len: usize,
        ranged: bool,
    ) -> DataTransportResult<StagingChunk> {
        let pool = self.pool(len as u64).await?;
        let mut headers = HeaderMap::new();
        headers.insert("if-match", selection.etag.clone());
        let expected = if ranged {
            let range = format!("bytes={offset}-{}", offset + len as u64 - 1);
            headers.insert(
                "range",
                HeaderValue::from_str(&range).expect("numeric range"),
            );
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        };
        let chunk = pool.reserve().await.map_err(unavailable)?;
        // A zero-byte object still needs a valid, nonempty NIXL descriptor.
        wire::bind(
            &mut headers,
            &pool.name,
            &pool.metadata,
            chunk.addr(),
            len.max(1),
        )
        .map_err(unavailable)?;
        let exposed = Exposed::new(chunk, pool.clone());
        let (response, exposed) = self
            .exchange(
                wire::request(
                    Method::GET,
                    selection.path,
                    selection.query.clone(),
                    headers,
                ),
                exposed,
            )
            .await?;
        if !response.status.is_success() {
            return Err(rejected(wire::s3_error(response.status, selection.path)));
        }
        if !response.headers.contains_key(RDMA_REPLY_HEADER) {
            // An ordinary successful GET returned the object in its HTTP body.
            // It is safe to try the ordinary reader before publishing any bytes.
            drop(exposed.finished());
            return Err(unavailable(Error::UnsupportedFeature(
                "The endpoint served GET over HTTP".into(),
            )));
        }
        if response.status != expected {
            return Err(rejected(Error::General(
                "Unexpected NIXL GET status".into(),
            )));
        }
        wire::receipt(expected, &response.headers, len as u64).map_err(rejected)?;
        let mut chunk = exposed.finished();
        if wire::field(&response.headers, "content-length")
            .map_err(rejected)?
            .is_some_and(|value| value != "0")
        {
            return Err(rejected(Error::General(
                "NIXL GET returned an HTTP payload".into(),
            )));
        }
        if ranged {
            let expected_range = format!(
                "bytes {offset}-{}/{}",
                offset + len as u64 - 1,
                selection.total
            );
            if wire::field(&response.headers, "content-range").map_err(rejected)?
                != Some(&expected_range)
            {
                return Err(rejected(Error::General(
                    "NIXL GET returned a different object range".into(),
                )));
            }
        }
        chunk.advance(len).map_err(native_error).map_err(rejected)?;
        Ok(chunk)
    }

    pub(super) async fn download_into(
        &self,
        request: DownloadRequest<'_>,
        mut target: Sink<'_>,
    ) -> DataTransportResult<DownloadResult> {
        if request.options.customer_key.is_some() {
            return Err(unavailable(Error::UnsupportedFeature(
                "NIXL does not encrypt SSE-C payloads in transit".into(),
            )));
        }
        let mut query = Vec::new();
        if let Some(version) = &request.options.version_id {
            query.push(("versionId".into(), version.clone()));
        }
        let mut headers = HeaderMap::new();
        if request.options.checksum_mode {
            headers.insert("x-amz-checksum-mode", HeaderValue::from_static("ENABLED"));
        }
        let head = self
            .control
            .execute_headers(wire::request(
                Method::HEAD,
                request.path,
                query.clone(),
                headers,
            ))
            .await
            .map_err(unavailable)?;
        if head.status != StatusCode::OK {
            return Err(rejected(wire::s3_error(head.status, request.path)));
        }
        let total = wire::field(&head.headers, "content-length")
            .map_err(rejected)?
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| rejected(Error::General("Missing or invalid object length".into())))?;
        let length = request.max_bytes.unwrap_or(total).min(total);
        if length > request.max_receive_bytes {
            return Err(rejected(Error::General(
                "NIXL object exceeds the receive bound".into(),
            )));
        }
        if query.is_empty()
            && let Some(version) =
                wire::field(&head.headers, "x-amz-version-id").map_err(rejected)?
        {
            query.push(("versionId".into(), version.into()));
        }
        let etag = head
            .headers
            .get("etag")
            .cloned()
            .ok_or_else(|| rejected(Error::General("Object HEAD has no ETag".into())))?;
        let selection = Selection {
            path: request.path,
            query,
            total,
            etag,
        };
        let ranged = length > 0 && (request.max_bytes.is_some() || length > WINDOW as u64);
        let first = length.min(WINDOW as u64) as usize;
        let mut current = self.fetch(&selection, 0, first, ranged).await?;
        let mut end = first as u64;
        let mut checksum = PayloadChecksumVerifier::new(&head.headers).map_err(rejected)?;
        loop {
            let write = async {
                if length == total {
                    checksum.update(current.staged().map_err(native_error)?);
                }
                let offset = end - current.len() as u64;
                match &mut target {
                    Sink::Writer(writer) => {
                        writer
                            .write_all(current.staged().map_err(native_error)?)
                            .await?
                    }
                    Sink::File(file) => {
                        use std::os::unix::fs::FileExt;
                        let file = file.clone();
                        tokio::task::spawn_blocking(move || {
                            file.write_all_at(current.staged().map_err(native_error)?, offset)?;
                            Ok::<_, Error>(())
                        })
                        .await
                        .map_err(|_| Error::General("NIXL file write was interrupted".into()))??;
                    }
                }
                (request.on_progress)(end, Some(length));
                Ok::<_, Error>(())
            };
            if end == length {
                write.await.map_err(rejected)?;
                break;
            }
            let size = (length - end).min(WINDOW as u64) as usize;
            let (written, next) = tokio::join!(write, self.fetch(&selection, end, size, true));
            written.map_err(rejected)?;
            current = next.map_err(|mut err| {
                err.state = DataTransferState::ConfirmedFailure;
                err
            })?;
            end += size as u64;
        }
        if length == total {
            checksum.verify(&head.headers).map_err(rejected)?;
        }
        if let Sink::Writer(writer) = target {
            writer
                .flush()
                .await
                .map_err(Error::from)
                .map_err(rejected)?;
        }
        Ok(DownloadResult {
            bytes_transferred: length,
            content_length: length,
        })
    }
}
