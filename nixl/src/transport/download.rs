use super::*;

pub(super) enum Sink<'a> {
    File(Arc<std::fs::File>),
    Writer(&'a mut (dyn AsyncWrite + Send + Unpin)),
}

impl Sink<'_> {
    async fn write_range(
        &mut self,
        mut window: Window,
        offset: u64,
        checksum: &mut Option<PayloadChecksumVerifier>,
    ) -> Result<()> {
        let len = window.len();
        match self {
            Self::Writer(writer) => {
                let mut chunks = window.buffer().received(len)?;
                while let Some(bytes) = chunks.next_chunk()? {
                    if let Some(checksum) = checksum.as_mut() {
                        checksum.update(bytes);
                    }
                    writer.write_all(bytes).await?;
                }
            }
            Self::File(file) => {
                use std::os::unix::fs::FileExt;
                let file = file.clone();
                let mut verifier = checksum.take();
                *checksum = tokio::task::spawn_blocking(move || {
                    if let Some(hash) = verifier.as_mut().filter(|c| c.needs_payload()) {
                        let mut position = offset;
                        let mut chunks = window.buffer().received(len)?;
                        while let Some(bytes) = chunks.next_chunk()? {
                            hash.update(bytes);
                            file.write_all_at(bytes, position)?;
                            position += bytes.len() as u64;
                        }
                    } else {
                        window.buffer().write_file(&file, offset, len)?;
                    }
                    Ok::<_, Error>(verifier)
                })
                .await
                .map_err(|_| Error::General("RDMA file write was interrupted".into()))??;
            }
        }
        Ok(())
    }
}

struct Selection<'a> {
    path: &'a RemotePath,
    requested_version: Option<&'a str>,
    total: u64,
    etag: HeaderValue,
    observed_version: Option<&'a str>,
}

fn read_request(
    method: Method,
    path: &RemotePath,
    version: Option<&str>,
    headers: HeaderMap,
) -> ControlRequest {
    let query = version
        .map(|value| ("versionId".into(), value.to_owned()))
        .into_iter()
        .collect();
    wire::request(method, path, query, headers)
}

// The caller may retry over HTTP only if both ranges declined before output.
pub(super) fn merged_ranges<T>(
    first: DataTransportResult<()>,
    second: DataTransportResult<T>,
) -> DataTransportResult<T> {
    match (first, second) {
        (Ok(()), Ok(value)) => Ok(value),
        (Err(first), Err(second)) => {
            if second.state == DataTransferState::OutcomeUnknown
                || first.state == DataTransferState::NotAccepted
            {
                Err(second)
            } else {
                Err(first)
            }
        }
        (Ok(()), Err(mut error)) | (Err(mut error), Ok(_)) => {
            if error.state == DataTransferState::NotAccepted {
                error.state = DataTransferState::ConfirmedFailure;
            }
            Err(error)
        }
    }
}

impl Transport {
    async fn fetch(
        &self,
        selection: &Selection<'_>,
        offset: u64,
        len: usize,
        ranged: bool,
        window: Window,
    ) -> DataTransportResult<Window> {
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
        let (response, exposed) = self
            .exchange(
                read_request(
                    Method::GET,
                    selection.path,
                    selection.requested_version,
                    headers,
                ),
                window,
                len,
            )
            .await?;
        if !response.status.is_success() {
            return Err(rejected(
                response.object_error(selection.path, selection.requested_version),
            ));
        }
        if !response.headers.contains_key(RDMA_REPLY_HEADER) {
            // An ordinary successful GET returned the object in its HTTP body.
            // It is safe to try the ordinary reader before publishing any bytes.
            // A missing receipt permits the S3 fallback but does not prove the window is idle.
            self.support.get.http_only();
            return Err(unavailable(Error::UnsupportedFeature(
                "The endpoint served GET over HTTP".into(),
            )));
        }
        if response.status != expected {
            return Err(rejected(Error::General(
                "Unexpected RDMA GET status".into(),
            )));
        }
        wire::receipt(expected, &response.headers, len as u64).map_err(rejected)?;
        self.support.get.supported();
        let mut window = exposed.finished().map_err(rejected)?;
        if wire::field(&response.headers, "content-length")
            .map_err(rejected)?
            .is_some_and(|value| value != "0")
        {
            return Err(rejected(Error::General(
                "RDMA GET returned an HTTP payload".into(),
            )));
        }
        if wire::field(&response.headers, "x-amz-version-id").map_err(rejected)?
            != selection.observed_version
        {
            return Err(rejected(Error::Conflict(format!(
                "Object version changed: {}",
                selection.path
            ))));
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
                    "RDMA GET returned a different object range".into(),
                )));
            }
        }
        window.finish_read(len).map_err(rejected)?;
        Ok(window)
    }

    pub(super) async fn download_into(
        &self,
        request: DownloadRequest<'_>,
        mut target: Sink<'_>,
    ) -> DataTransportResult<DownloadResult> {
        if request.options.customer_key.is_some() {
            return Err(unavailable(Error::UnsupportedFeature(
                "RDMA does not encrypt SSE-C payloads in transit".into(),
            )));
        }
        let _probe = self.support.get.enter().await?;
        let requested_version = request.options.version_id.as_deref();
        let mut headers = HeaderMap::new();
        if request.options.checksum_mode {
            headers.insert("x-amz-checksum-mode", HeaderValue::from_static("ENABLED"));
        }
        let head = self
            .control
            .execute_headers(read_request(
                Method::HEAD,
                request.path,
                requested_version,
                headers,
            ))
            .await
            .map_err(unavailable)?;
        if head.status != StatusCode::OK {
            return Err(rejected(head.object_error(request.path, requested_version)));
        }
        let total = wire::field(&head.headers, "content-length")
            .map_err(rejected)?
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| rejected(Error::General("Missing or invalid object length".into())))?;
        let length = request.max_bytes.unwrap_or(total).min(total);
        if length > request.max_receive_bytes {
            return Err(rejected(Error::General(
                "RDMA object exceeds the receive bound".into(),
            )));
        }
        let observed_version = wire::field(&head.headers, "x-amz-version-id").map_err(rejected)?;
        let etag = head
            .headers
            .get("etag")
            .cloned()
            .ok_or_else(|| rejected(Error::General("Object HEAD has no ETag".into())))?;
        let selection = Selection {
            path: request.path,
            requested_version,
            total,
            etag,
            observed_version,
        };
        let ranged = length > 0 && (request.max_bytes.is_some() || length > WINDOW as u64);
        let verifier = PayloadChecksumVerifier::new(&head.headers).map_err(rejected)?;
        let mut checksum = (length == total).then_some(verifier);
        let mut offset = 0;
        loop {
            let first = (length - offset).min(WINDOW as u64);
            let next = offset + first;
            let second = (length - next).min(WINDOW as u64);
            // Reserve the earlier range first. It can finish and release its window
            // even if other downloads or a quarantined window leave only one slot.
            let window = self.pools.reserve_read(first, total).await.map_err(|e| {
                if offset == 0 {
                    unavailable(e)
                } else {
                    rejected(e)
                }
            })?;
            let publish_first = async {
                let window = self
                    .fetch(&selection, offset, first as usize, ranged, window)
                    .await
                    .map_err(|mut error| {
                        if offset != 0 && error.state == DataTransferState::NotAccepted {
                            error.state = DataTransferState::ConfirmedFailure;
                        }
                        error
                    })?;
                target
                    .write_range(window, offset, &mut checksum)
                    .await
                    .map_err(rejected)?;
                (request.on_progress)(next, Some(length));
                Ok(())
            };
            if second == 0 {
                publish_first.await?;
                break;
            }
            let fetch_second = async {
                let window = self
                    .pools
                    .reserve_read(second, total)
                    .await
                    .map_err(unavailable)?;
                self.fetch(&selection, next, second as usize, true, window)
                    .await
            };
            // Settle both control exchanges even when one fails. Only the ordered
            // consumer updates the checksum, writes bytes and reports progress.
            let (first_result, second_result) = tokio::join!(publish_first, fetch_second);
            let window = merged_ranges(first_result, second_result)?;
            target
                .write_range(window, next, &mut checksum)
                .await
                .map_err(rejected)?;
            offset = next + second;
            (request.on_progress)(offset, Some(length));
            if offset == length {
                break;
            }
        }
        if let Some(checksum) = checksum {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt;

    #[tokio::test]
    async fn direct_and_checksummed_file_ranges_preserve_offsets() {
        let data: Vec<_> = (0_u8..=250).cycle().take(300_005).collect();
        let source = tempfile::tempfile().unwrap();
        source.write_all_at(&data, 7).unwrap();
        let pools = Pools::new(RdmaBackend::Nixl);
        for digest in [None, Some("AJedqGnpgDo/yTu1OpwK3pwDFbPXFlooiAFQy8h5Lq0=")] {
            let mut window = pools.reserve(data.len() as u64).await.unwrap();
            window.buffer().read_file(&source, 7, data.len()).unwrap();
            window.finish_read(data.len()).unwrap();
            let file = Arc::new(tempfile::tempfile().unwrap());
            file.write_all_at(b"prefix", 0).unwrap();
            let mut headers = HeaderMap::new();
            if let Some(value) = digest {
                headers.insert("x-amz-checksum-sha256", HeaderValue::from_static(value));
            }
            let mut checksum = Some(PayloadChecksumVerifier::new(&headers).unwrap());
            Sink::File(file.clone())
                .write_range(window, 6, &mut checksum)
                .await
                .unwrap();
            let result = checksum.unwrap().verify(&headers).unwrap();
            assert_eq!(
                matches!(result, PayloadChecksumStatus::Verified(_)),
                digest.is_some()
            );
            let mut actual = vec![0; 6 + data.len()];
            file.read_exact_at(&mut actual, 0).unwrap();
            assert_eq!(&actual[..6], b"prefix");
            assert!(actual[6..] == data);
        }
    }
}
