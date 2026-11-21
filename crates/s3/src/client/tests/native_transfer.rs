use super::*;
use rc_core::{DataTransferState, DataTransportMode};

#[tokio::test]
async fn direct_download_replaces_destination_only_after_length_verification() {
    for (outcome, expected_bytes) in [
        (Ok((4, 4)), Some(4)),
        (Ok((3, 4)), None),
        (Ok((5, 5)), None),
        (Err(DataTransferState::NotAccepted), None),
        (Err(DataTransferState::ConfirmedFailure), None),
    ] {
        let (client, replay) = test_s3_client_with_response_sequence(vec![]);
        let client = client.with_object_data_transport(
            DataTransportMode::Auto,
            external_transport(ExternalBehavior::DirectDownload(outcome)),
        );
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("object");
        std::fs::write(&destination, b"original").unwrap();

        let result = client
            .download_object_to_path(
                &RemotePath::new("test", "bucket", "key"),
                &destination,
                |_, _| {},
            )
            .await;

        let contents = std::fs::read(&destination).unwrap();
        if let Some(bytes) = expected_bytes {
            assert_eq!(result.unwrap(), bytes);
            assert_eq!(contents, b"file");
        } else {
            assert!(result.is_err(), "{outcome:?}");
            assert_eq!(contents, b"original", "{outcome:?}");
        }
        assert_eq!(replay.actual_requests().count(), 0, "{outcome:?}");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}

fn assert_multipart_requests(replay: &StaticReplayClient, expected: &[(&str, &str)]) {
    let requests: Vec<_> = replay.actual_requests().collect();
    assert_eq!(requests.len(), expected.len());
    for (request, &(method, query)) in requests.iter().zip(expected) {
        assert_eq!(request.method(), method);
        assert!(request.uri().contains(query), "{}", request.uri());
    }
}

#[tokio::test]
async fn accepted_probe_uploads_the_part_and_completes() {
    let (client, replay) = test_s3_client_with_response_sequence(vec![
        multipart_create_response(),
        multipart_copy_complete_response(),
    ]);
    let provider = external_transport(ExternalBehavior::Probe(Ok(true)));
    let client = client.with_object_data_transport(DataTransportMode::Auto, provider.clone());
    let path = RemotePath::new("test", "bucket", "key");

    let object = client
        .put_object(&path, b"data".to_vec(), None, None)
        .await
        .unwrap();

    assert_eq!(object.etag.as_deref(), Some("complete-etag"));
    assert_eq!(
        provider.operations.lock().unwrap().first().unwrap(),
        "probe:upload-id:4"
    );
    assert_eq!(*provider.uploaded_payloads.lock().unwrap(), [b"data"]);
    assert_multipart_requests(
        &replay,
        &[("POST", "?uploads"), ("POST", "uploadId=upload-id")],
    );
}

#[tokio::test]
async fn probe_fallback_requires_an_untouched_reader_and_successful_abort() {
    for (probe, abort_status) in [
        (Ok(false), 204),
        (Err(DataTransferState::NotAccepted), 204),
        (Err(DataTransferState::OutcomeUnknown), 204),
        (Ok(false), 403),
        (Err(DataTransferState::NotAccepted), 403),
    ] {
        let abort = http::Response::builder()
            .status(abort_status)
            .body(SdkBody::empty())
            .unwrap();
        let (client, replay) =
            test_s3_client_with_response_sequence(vec![multipart_create_response(), abort]);
        let provider = external_transport(ExternalBehavior::Probe(probe));
        let client = client.with_object_data_transport(DataTransportMode::Auto, provider.clone());
        let mut payload = b"data".as_slice();
        let mut source = ObservedReader {
            inner: &mut payload,
            polled: false,
            bytes: 0,
            eof: false,
        };
        let path = RemotePath::new("test", "bucket", "key");
        let request = UploadRequest {
            path: &path,
            options: &ObjectWriteOptions::default(),
            operation: UploadOperation::PutObject {
                condition: WriteCondition::None,
            },
            content_length: 4,
            checksum_sha256: None,
        };

        let result = client
            .try_external_upload_reader(request, &mut source)
            .await;

        assert!(!source.polled, "probe {probe:?}, abort {abort_status}");
        if abort_status == 403 {
            assert!(
                matches!(result, Err(Error::General(message)) if message.contains("abort: failed"))
            );
        } else if probe == Err(DataTransferState::OutcomeUnknown) {
            assert!(matches!(
                result,
                Err(Error::DataTransfer {
                    state: DataTransferState::OutcomeUnknown,
                    ..
                })
            ));
        } else {
            assert!(
                matches!(result, Ok(None)),
                "HTTP fallback must remain available"
            );
        }
        assert_eq!(*provider.operations.lock().unwrap(), ["probe:upload-id:4"]);
        assert_multipart_requests(
            &replay,
            &[("POST", "?uploads"), ("DELETE", "uploadId=upload-id")],
        );
    }
}

#[tokio::test]
async fn failed_file_part_waits_for_the_outstanding_receipt_before_abort() {
    let abort = http::Response::builder()
        .status(204)
        .body(SdkBody::empty())
        .unwrap();
    let (client, replay) =
        test_s3_client_with_response_sequence(vec![multipart_create_response(), abort]);
    let control = Arc::new(FilePartFailure::default());
    let provider = external_transport(ExternalBehavior::FailSecondFilePart(control.clone()));
    let client = client.with_object_data_transport(DataTransportMode::Rdma, provider.clone());
    let file = tempfile::NamedTempFile::new().unwrap();
    file.as_file()
        .set_len(crate::multipart::DEFAULT_PART_SIZE + 1)
        .unwrap();
    let path = RemotePath::new("test", "bucket", "key");
    let options = ObjectWriteOptions::default();
    let upload = client.put_object_from_path_with_options(&path, file.path(), &options, |_| {});
    tokio::pin!(upload);

    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            biased;
            result = &mut upload => panic!("upload returned before the first receipt: {result:?}"),
            () = control.second_failed.notified() => {}
        }
        assert_multipart_requests(&replay, &[("POST", "?uploads")]);
        control.release_first.notify_one();
        let error = upload
            .await
            .expect_err("the failed second part must abort the upload");
        assert!(matches!(
            error,
            Error::DataTransfer {
                state: DataTransferState::ConfirmedFailure,
                ..
            }
        ));
    })
    .await
    .expect("the upload must finish after the receipt is released");

    assert_eq!(*provider.operations.lock().unwrap(), ["receipt:1"]);
    assert_multipart_requests(
        &replay,
        &[("POST", "?uploads"), ("DELETE", "uploadId=upload-id")],
    );
}
