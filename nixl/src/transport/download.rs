use super::*;

pub(super) enum Sink<'a> {
    File(Arc<std::fs::File>),
    Writer(&'a mut (dyn AsyncWrite + Send + Unpin)),
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

impl Transport {
    async fn fetch(
        &self,
        selection: &Selection<'_>,
        offset: u64,
        len: usize,
        ranged: bool,
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
        let window = self.pools.reserve(len as u64).await.map_err(unavailable)?;
        // An empty object still requires a nonempty descriptor.
        let exposed = window
            .expose(&Method::GET, len.max(1), &mut headers)
            .map_err(unavailable)?;
        let (response, exposed) = self
            .exchange(
                read_request(
                    Method::GET,
                    selection.path,
                    selection.requested_version,
                    headers,
                ),
                exposed,
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
            drop(exposed.finished());
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
        let mut window = exposed.finished();
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
        let first = length.min(WINDOW as u64) as usize;
        let mut current = self.fetch(&selection, 0, first, ranged).await?;
        let mut end = first as u64;
        let mut checksum = PayloadChecksumVerifier::new(&head.headers).map_err(rejected)?;
        loop {
            let write = async {
                if length == total {
                    checksum.update(current.received()?);
                }
                let offset = end - current.len() as u64;
                match &mut target {
                    Sink::Writer(writer) => writer.write_all(current.received()?).await?,
                    Sink::File(file) => {
                        use std::os::unix::fs::FileExt;
                        let file = file.clone();
                        tokio::task::spawn_blocking(move || {
                            file.write_all_at(current.received()?, offset)?;
                            Ok::<_, Error>(())
                        })
                        .await
                        .map_err(|_| Error::General("RDMA file write was interrupted".into()))??;
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
